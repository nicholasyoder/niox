//! The PipeWire side of niox audio: a thread of its own running a
//! PipeWire main loop in the user's session, with
//!
//! - **the niox sink**, while the [`Plan`] has hosts to send to: a
//!   stereo `Audio/Sink` node, made the default output, whose audio goes
//!   to each host. It's a stream of this process, so it exists exactly as
//!   long as the process does: if niox-session dies, the session manager
//!   moves everything playing into it back to a real output. Because apps
//!   play into it whether or not the machine has speakers, nothing depends
//!   on audio hardware.
//! - **a player per client**, once that client's audio arrives: a
//!   playback stream into the default output, removed again once the
//!   client has been quiet for a few seconds.
//!
//! The default output is set through the `default` metadata's
//! `default.configured.audio.sink`, the key `pactl set-default-sink` and
//! the desktop's sound settings write. When the sink goes away cleanly,
//! the previous choice is put back (unless the user picked another one in
//! the meantime). After a crash the key names a sink that no longer
//! exists, so the session manager falls back to the best real one, and
//! picks the niox sink again when niox-session comes back.
//!
//! If PipeWire isn't running, or goes away, the thread retries with
//! backoff; the plan is kept and applied once it's back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::context::ContextRc;
use pw::core::CoreRc;
use pw::main_loop::{MainLoopRc, MainLoopWeak};
use pw::metadata::{Metadata, MetadataListener};
use pw::properties::properties;
use pw::registry::RegistryRc;
use pw::spa;
use pw::stream::{StreamListener, StreamRc, StreamState};
use pw::types::ObjectType;
use spa::utils::result::AsyncSeq;
use tracing::{debug, info, warn};

use crate::Receiver;
use crate::streams::{Delivery, FORMAT, Fader, Out, Plan, Streams};

/// The niox sink's node name, which the default-output setting refers to.
pub const SINK_NAME: &str = "niox_sink";

/// The metadata key holding the user's chosen default output.
const DEFAULT_SINK_KEY: &str = "default.configured.audio.sink";

const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(10);

/// How long a clean shutdown waits for PipeWire to take the default
/// output back.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

/// The PipeWire core's object id, for its errors.
const PW_ID_CORE: u32 = 0;

/// Linux's `EPIPE`: the core's error when PipeWire hangs up.
const EPIPE: i32 = 32;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    #[error(transparent)]
    Audio(#[from] crate::Error),
    #[error("starting the PipeWire thread")]
    Thread(#[source] std::io::Error),
}

enum Command {
    Plan,
    /// A client's audio has arrived and it has no player yet.
    Want(String),
    Quit,
}

/// Runs the audio streams in the user's PipeWire. See the module docs.
///
/// Cheap to share: every method takes `&self` and may be called from any
/// thread. Dropping it shuts down cleanly ([`Engine::shutdown`]).
pub struct Engine {
    streams: Arc<Streams>,
    commands: Arc<Mutex<Option<pw::channel::Sender<Command>>>>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("streams", &self.streams)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Starts the PipeWire thread with an empty plan. Bodies for the
    /// network go to `out`, which must never block.
    pub fn start(out: Out) -> Result<Self, EngineError> {
        static INIT: Once = Once::new();
        INIT.call_once(pw::init);

        let streams = Arc::new(Streams::new(out)?);
        let commands = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (streams, commands, stop) = (
                Arc::clone(&streams),
                Arc::clone(&commands),
                Arc::clone(&stop),
            );
            thread::Builder::new()
                .name("pipewire".into())
                .spawn(move || run(&streams, &commands, &stop))
                .map_err(EngineError::Thread)?
        };
        Ok(Self {
            streams,
            commands,
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Switches to `plan`, keeping whatever streams it still wants.
    pub fn set_plan(&self, plan: Plan) {
        if self.streams.plan() != plan {
            self.streams.set_plan(plan);
            self.command(Command::Plan);
        }
    }

    /// Hands an audio body from `peer` to its stream. Never blocks.
    pub fn deliver(&self, peer: &str, body: &[u8]) {
        if self.streams.deliver(peer, body) == Delivery::Want {
            self.command(Command::Want(peer.to_owned()));
        }
    }

    /// Removes the niox sink (giving the default output back) and every
    /// player, and stops the thread. Waits up to about a second.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.command(Command::Quit);
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(thread) = thread {
            thread.thread().unpark();
            if thread.join().is_err() {
                warn!("the PipeWire thread panicked");
            }
        }
    }

    fn command(&self, command: Command) {
        if let Some(tx) = &*self.commands.lock().unwrap_or_else(PoisonError::into_inner) {
            // Fails only if the loop is gone, which is about to be noticed.
            let _ = tx.send(command);
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The thread: connects to PipeWire and runs the plan until told to stop,
/// reconnecting whenever the connection fails.
fn run(
    streams: &Arc<Streams>,
    commands: &Mutex<Option<pw::channel::Sender<Command>>>,
    stop: &AtomicBool,
) {
    let mut retry = RETRY_MIN;
    let mut failing: Option<String> = None;
    while !stop.load(Ordering::SeqCst) {
        match session(streams, commands, stop) {
            Ok(()) => {
                failing = None;
                retry = RETRY_MIN;
                if !stop.load(Ordering::SeqCst) {
                    info!("lost the connection to PipeWire; reconnecting");
                }
            }
            Err(e) => {
                let message = format!("connecting to PipeWire: {e}");
                if failing.as_deref() != Some(&message) {
                    warn!("{message}; retrying until it's available");
                } else {
                    debug!("{message}");
                }
                failing = Some(message);
            }
        }
        *commands.lock().unwrap_or_else(PoisonError::into_inner) = None;
        if stop.load(Ordering::SeqCst) {
            break;
        }
        thread::park_timeout(retry);
        retry = (retry * 2).min(RETRY_MAX);
    }
}

/// What the loop has made, on the PipeWire thread.
struct State {
    core: CoreRc,
    streams: Arc<Streams>,
    sink: Option<Sink>,
    players: HashMap<String, Player>,
    default: DefaultSink,
    /// The sync that tells the metadata's first values are in.
    metadata_sync: Option<AsyncSeq>,
    /// Shutting down: the sync that says PipeWire has seen everything, and
    /// when to give up waiting for it.
    quitting: Option<(Option<AsyncSeq>, Instant)>,
}

/// The niox sink. Fields drop in order: listener first.
struct Sink {
    _listener: StreamListener<Vec<f32>>,
    _stream: StreamRc,
}

struct Player {
    _listener: StreamListener<Playback>,
    _stream: StreamRc,
    receiver: Arc<Receiver>,
}

/// A metadata value and its type.
type Value = (Option<String>, String);

/// Making the niox sink the default output, and giving it back.
#[derive(Default)]
struct DefaultSink {
    metadata: Option<(Metadata, MetadataListener)>,
    /// The `default` metadata has reported its current values.
    synced: bool,
    /// The current value of [`DEFAULT_SINK_KEY`] (type, value).
    configured: Rc<RefCell<Option<Value>>>,
    /// While the niox sink is the default: what it replaced.
    replaced: Option<Option<Value>>,
}

impl DefaultSink {
    fn ours() -> String {
        format!("{{\"name\":\"{SINK_NAME}\"}}")
    }

    fn is_ours(value: Option<&Value>) -> bool {
        value.is_some_and(|(_, v)| json_name(v).as_deref() == Some(SINK_NAME))
    }

    /// Makes the niox sink the default, once the metadata is known.
    fn claim(&mut self) {
        let Some((metadata, _)) = &self.metadata else {
            return;
        };
        if !self.synced || self.replaced.is_some() {
            return;
        }
        let current = self.configured.borrow().clone();
        // After a crash the key still names the niox sink, and what it
        // replaced is lost; there's nothing better to put back than
        // nothing, which lets the session manager choose.
        self.replaced = Some(current.filter(|c| !Self::is_ours(Some(c))));
        let ours = (Some("Spa:String:JSON".to_owned()), Self::ours());
        metadata.set_property(0, DEFAULT_SINK_KEY, ours.0.as_deref(), Some(&ours.1));
        // As PipeWire will echo it, so a release that comes first still
        // sees it as ours.
        *self.configured.borrow_mut() = Some(ours);
        info!("niox sink is the default output");
    }

    /// Puts back what the niox sink replaced, unless the user has chosen
    /// something else since.
    fn release(&mut self) {
        let Some(replaced) = self.replaced.take() else {
            return;
        };
        let Some((metadata, _)) = &self.metadata else {
            return;
        };
        if !Self::is_ours(self.configured.borrow().as_ref()) {
            return;
        }
        match &replaced {
            Some((type_, value)) => {
                metadata.set_property(0, DEFAULT_SINK_KEY, type_.as_deref(), Some(value));
            }
            None => metadata.set_property(0, DEFAULT_SINK_KEY, None, None),
        }
        info!("gave the default output back");
    }
}

/// The `name` in a `{"name": "..."}` metadata value.
fn json_name(value: &str) -> Option<String> {
    let rest = value.split_once("\"name\"")?.1;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest.split_once('"')?.0.to_owned())
}

/// One connection to PipeWire, until it fails or the engine stops.
fn session(
    streams: &Arc<Streams>,
    commands: &Mutex<Option<pw::channel::Sender<Command>>>,
    stop: &AtomicBool,
) -> Result<(), pw::Error> {
    let mainloop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(Some(properties! {
        *pw::keys::APP_NAME => "niox",
    }))?;
    info!("connected to PipeWire");

    let state = Rc::new(RefCell::new(State {
        core: core.clone(),
        streams: Arc::clone(streams),
        sink: None,
        players: HashMap::new(),
        default: DefaultSink::default(),
        metadata_sync: None,
        quitting: None,
    }));

    // PipeWire going away ends the session.
    let _core_listener = core
        .add_listener_local()
        .error({
            let mainloop = mainloop.downgrade();
            move |id, _seq, res, message| {
                if id == PW_ID_CORE && res == -EPIPE {
                    quit(&mainloop);
                } else {
                    warn!(id, res, "PipeWire: {message}");
                }
            }
        })
        .done({
            let (state, mainloop) = (Rc::downgrade(&state), mainloop.downgrade());
            move |id, seq| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                if id != PW_ID_CORE {
                    return;
                }
                let mut state = state.borrow_mut();
                if state.quitting.is_some_and(|(q, _)| q == Some(seq)) {
                    quit(&mainloop);
                } else if state.metadata_sync == Some(seq) {
                    state.default.synced = true;
                    if state.sink.is_some() {
                        state.default.claim();
                    }
                }
            }
        })
        .register();

    let registry = core.get_registry_rc()?;
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (state, registry) = (Rc::downgrade(&state), registry.downgrade());
            move |global| {
                let is_default = global.type_ == ObjectType::Metadata
                    && global
                        .props
                        .is_some_and(|p| p.get("metadata.name") == Some("default"));
                if let (true, Some(state), Some(registry)) =
                    (is_default, state.upgrade(), registry.upgrade())
                {
                    bind_default(&state, &registry, global);
                }
            }
        })
        .register();

    let (tx, rx) = pw::channel::channel();
    *commands.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    let _commands = rx.attach(mainloop.loop_(), {
        let (state, mainloop) = (Rc::downgrade(&state), mainloop.downgrade());
        move |command| {
            let Some(state) = state.upgrade() else {
                return;
            };
            match command {
                Command::Plan => apply_plan(&state),
                Command::Want(peer) => add_player(&state, &peer),
                Command::Quit => shut_down(&state, &mainloop),
            }
        }
    });

    // Once a second: remove quiet clients' players, and give up on a
    // shutdown PipeWire never confirmed.
    let tick = Duration::from_secs(1);
    let timer = mainloop.loop_().add_timer({
        let (state, mainloop) = (Rc::downgrade(&state), mainloop.downgrade());
        move |_| {
            let Some(state) = state.upgrade() else {
                return;
            };
            if state
                .borrow()
                .quitting
                .is_some_and(|(_, deadline)| Instant::now() >= deadline)
            {
                quit(&mainloop);
                return;
            }
            let gone = state.borrow().streams.idle_check();
            let mut state = state.borrow_mut();
            for peer in gone {
                if state.players.remove(&peer).is_some() {
                    info!(peer, "stopped playing a quiet client's audio");
                }
            }
        }
    });
    let _ = timer.update_timer(Some(tick), Some(tick));

    // A stop that came before the channel was in place.
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    // Receivers left from a previous connection have no players.
    streams.clear_receivers();
    apply_plan(&state);
    mainloop.run();

    let mut state = state.borrow_mut();
    state.players.clear();
    state.sink = None;
    streams.clear_receivers();
    Ok(())
}

fn quit(mainloop: &MainLoopWeak) {
    if let Some(mainloop) = mainloop.upgrade() {
        mainloop.quit();
    }
}

/// Binds the `default` metadata, tracks the configured default output,
/// and syncs so the first values are in before the sink claims it.
fn bind_default(
    state: &Rc<RefCell<State>>,
    registry: &RegistryRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
) {
    let metadata: Metadata = match registry.bind(global) {
        Ok(m) => m,
        Err(e) => {
            warn!("binding PipeWire's default metadata: {e}");
            return;
        }
    };
    let configured = Rc::clone(&state.borrow().default.configured);
    let listener = metadata
        .add_listener_local()
        .property(move |subject, key, type_, value| {
            if subject == 0 && (key == Some(DEFAULT_SINK_KEY) || key.is_none()) {
                *configured.borrow_mut() = value.map(|v| (type_.map(str::to_owned), v.to_owned()));
            }
            0
        })
        .register();
    let mut state = state.borrow_mut();
    state.default.metadata = Some((metadata, listener));
    state.default.synced = false;
    match state.core.sync(0) {
        Ok(seq) => state.metadata_sync = Some(seq),
        Err(e) => warn!("syncing with PipeWire: {e}"),
    }
}

/// Brings the sink and players in line with the plan.
fn apply_plan(state: &Rc<RefCell<State>>) {
    let plan = state.borrow().streams.plan();
    // `Streams` has already dropped the receivers the plan no longer
    // wants (or wants at another latency); their players go too.
    {
        let mut s = state.borrow_mut();
        let streams = Arc::clone(&s.streams);
        s.players
            .retain(|peer, player| streams.is_current(peer, &player.receiver));
    }
    let want_sink = !plan.send_to.is_empty();
    let have_sink = state.borrow().sink.is_some();
    if want_sink && !have_sink {
        match make_sink(state, &plan) {
            Ok(sink) => {
                let mut s = state.borrow_mut();
                s.sink = Some(sink);
                s.default.claim();
            }
            Err(e) => warn!("making the niox sink: {e}"),
        }
    } else if !want_sink && have_sink {
        let mut s = state.borrow_mut();
        s.default.release();
        s.sink = None;
        info!("removed the niox sink");
    }
}

fn sink_description(plan: &Plan) -> String {
    match plan.send_to.as_slice() {
        [host] => format!("niox → {host}"),
        _ => "niox".into(),
    }
}

/// The stream format both kinds of stream take.
fn format_param() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(FORMAT.rate);
    info.set_channels(2);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default()
}

const FRAME_BYTES: usize = 2 * size_of::<f32>();

/// A player's state on the PipeWire thread.
struct Playback {
    samples: Vec<f32>,
    fader: Fader,
}

fn make_sink(state: &Rc<RefCell<State>>, plan: &Plan) -> Result<Sink, pw::Error> {
    let core = state.borrow().core.clone();
    let streams = Arc::clone(&state.borrow().streams);
    let stream = StreamRc::new(
        core,
        SINK_NAME,
        properties! {
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => SINK_NAME,
            *pw::keys::NODE_DESCRIPTION => sink_description(plan),
            *pw::keys::MEDIA_ICON_NAME => "network-wired",
        },
    )?;
    let listener = stream
        .add_local_listener_with_user_data(Vec::<f32>::new())
        .state_changed(|_, _, old, new| {
            if let StreamState::Error(e) = &new {
                warn!("niox sink: {e}");
            } else {
                debug!(?old, ?new, "niox sink");
            }
        })
        .process(move |stream, samples| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(bytes) = data.data() else {
                return;
            };
            let Some(bytes) = bytes.get(offset..offset + size) else {
                return;
            };
            let bytes = &bytes[..bytes.len() / FRAME_BYTES * FRAME_BYTES];
            samples.clear();
            samples.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b)),
            );
            if !samples.is_empty() {
                streams.capture(samples);
            }
        })
        .register()?;
    let format = format_param();
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or(pw::Error::CreationFailed)?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    info!(hosts = ?plan.send_to, "made the niox sink");
    Ok(Sink {
        _listener: listener,
        _stream: stream,
    })
}

fn add_player(state: &Rc<RefCell<State>>, peer: &str) {
    if state.borrow().players.contains_key(peer) {
        return;
    }
    let streams = Arc::clone(&state.borrow().streams);
    let receiver = match streams.add_receiver(peer) {
        Ok(Some(receiver)) => receiver,
        Ok(None) => return,
        Err(e) => {
            warn!(peer, "can't play this client's audio: {e}");
            return;
        }
    };
    match make_player(state, peer, receiver) {
        Ok(player) => {
            info!(peer, "a client's audio arrived; playing it");
            state.borrow_mut().players.insert(peer.to_owned(), player);
        }
        Err(e) => warn!(peer, "making a player for this client: {e}"),
    }
}

fn make_player(
    state: &Rc<RefCell<State>>,
    peer: &str,
    receiver: Arc<Receiver>,
) -> Result<Player, pw::Error> {
    let core = state.borrow().core.clone();
    let streams = Arc::clone(&state.borrow().streams);
    let stream = StreamRc::new(
        core,
        &format!("niox: {peer}"),
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_NAME => peer,
            *pw::keys::NODE_NAME => format!("niox_from_{peer}"),
            *pw::keys::NODE_DESCRIPTION => format!("niox: {peer}"),
            *pw::keys::APP_NAME => "niox",
        },
    )?;
    let playback = Playback {
        samples: Vec::new(),
        // Starts where the plan has it: a muted client starts silent.
        fader: Fader::new(streams.gain(peer)),
    };
    let peer = peer.to_owned();
    let playing = Arc::clone(&receiver);
    let listener = stream
        .add_local_listener_with_user_data(playback)
        .state_changed({
            let peer = peer.clone();
            move |_, _, old, new| {
                if let StreamState::Error(e) = &new {
                    warn!(peer, "player: {e}");
                } else {
                    debug!(peer, ?old, ?new, "player");
                }
            }
        })
        .process(move |stream, playback| {
            let Playback { samples, fader } = playback;
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = usize::try_from(buffer.requested()).unwrap_or(usize::MAX);
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let Some(bytes) = data.data() else {
                return;
            };
            let mut frames = bytes.len() / FRAME_BYTES;
            if requested > 0 {
                frames = frames.min(requested);
            }
            samples.resize(frames * 2, 0.0);
            streams.play(&peer, &playing, fader, samples);
            for (out, sample) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(samples.iter()) {
                *out = sample.to_le_bytes();
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = FRAME_BYTES as i32;
            *chunk.size_mut() = (frames * FRAME_BYTES) as u32;
        })
        .register()?;
    let format = format_param();
    let mut params = [spa::pod::Pod::from_bytes(&format).ok_or(pw::Error::CreationFailed)?];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    Ok(Player {
        _listener: listener,
        _stream: stream,
        receiver,
    })
}

/// A clean stop: give the default output back, remove everything, and
/// quit once PipeWire has seen it (a round trip), or after
/// [`SHUTDOWN_WAIT`].
fn shut_down(state: &Rc<RefCell<State>>, mainloop: &MainLoopWeak) {
    let mut s = state.borrow_mut();
    s.players.clear();
    s.default.release();
    s.sink = None;
    match s.core.sync(1) {
        Ok(seq) => s.quitting = Some((Some(seq), Instant::now() + SHUTDOWN_WAIT)),
        Err(_) => {
            drop(s);
            quit(mainloop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_names() {
        assert_eq!(
            json_name(r#"{ "name": "alsa_output.pci" }"#).as_deref(),
            Some("alsa_output.pci")
        );
        assert_eq!(json_name(&DefaultSink::ours()).as_deref(), Some(SINK_NAME));
        assert_eq!(json_name("{}"), None);
        assert!(DefaultSink::is_ours(Some(&(None, DefaultSink::ours()))));
        assert!(!DefaultSink::is_ours(Some(&(
            None,
            r#"{"name":"alsa_output.pci"}"#.into()
        ))));
        assert!(!DefaultSink::is_ours(None));
    }
}
