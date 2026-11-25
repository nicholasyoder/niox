//! The host side: grab the local keyboards and mice, run the hotkey and
//! focus logic, and send frames to the focused client (or play them on
//! this machine's own passthrough device).

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use niox_config::Hotkey;
use niox_km::device::{
    Capture, CaptureEnd, InputDevice, Stop, VirtualDevice, discover, union_capabilities,
};
use niox_km::{HostKm, HostOutput, Message, Target};
use niox_transport::{Event, Handle, PeerId};
use tracing::{debug, error, info, warn};

use super::{lock, send};

/// How long to wait for held keys to be released before grabbing a device.
const SETTLE: Duration = Duration::from_secs(2);
/// Longest the timer thread sleeps between checks for overdue
/// capabilities resends.
const TICK: Duration = Duration::from_millis(100);

/// What the capture threads, the timer thread, and the transport handler
/// share. Each locks it for one step and carries out the outputs before
/// unlocking, so frames reach each client in the order they were
/// sequenced.
#[derive(Debug)]
pub struct State {
    km: HostKm<PeerId>,
    /// Plays input on this machine while it has focus.
    local: VirtualDevice,
    out: Vec<HostOutput<PeerId>>,
    buf: Vec<u8>,
}

impl State {
    /// Runs `step` on the KM state, then carries out what it produced.
    fn step(
        &mut self,
        handle: &Handle,
        step: impl FnOnce(&mut HostKm<PeerId>, &mut Vec<HostOutput<PeerId>>),
    ) {
        step(&mut self.km, &mut self.out);
        let mut out = std::mem::take(&mut self.out);
        for output in out.drain(..) {
            match output {
                HostOutput::Local(events) => {
                    if let Err(e) = self.local.play(&events) {
                        warn!("{:#}", anyhow::Error::from(e));
                    }
                }
                HostOutput::Send { peer, message } => {
                    send(handle, &mut self.buf, peer, &message);
                }
                HostOutput::Focus(Target::Host) => info!(focus = "host", "focus moved"),
                HostOutput::Focus(Target::Client(peer)) => {
                    info!(focus = handle.peer_name(peer), "focus moved");
                }
            }
        }
        self.out = out;
    }

    fn is_ready(&self, peer: PeerId) -> bool {
        self.km.ready_clients().any(|p| p == peer)
    }
}

/// Handles a transport event for a peer whose role is `client`.
pub fn on_event(state: &Mutex<State>, handle: &Handle, event: Event<'_>) {
    let mut s = lock(state);
    match event {
        Event::Up(peer) => s.step(handle, |km, out| km.peer_up(peer, Instant::now(), out)),
        Event::Down(peer) => {
            let was_ready = s.is_ready(peer);
            s.step(handle, |km, out| km.peer_down(peer, out));
            if was_ready {
                info!(
                    client = handle.peer_name(peer),
                    "client left the focus cycle"
                );
            }
        }
        Event::Data { peer, body, .. } => {
            let message = match Message::decode(body) {
                Ok(m) => m,
                Err(e) => {
                    debug!(peer = handle.peer_name(peer), "dropped a KM message: {e}");
                    return;
                }
            };
            let was_ready = s.is_ready(peer);
            if let Err(e) = s.km.on_message(peer, &message) {
                debug!(peer = handle.peer_name(peer), ?message, "{e}");
            }
            if !was_ready && s.is_ready(peer) {
                info!(
                    client = handle.peer_name(peer),
                    "client joined the focus cycle"
                );
            }
        }
    }
}

/// The host side's devices and threads.
#[derive(Debug)]
pub struct Host {
    state: Arc<Mutex<State>>,
    /// Discovered devices, until [`Host::start`] hands them to their
    /// capture threads.
    devices: Vec<InputDevice>,
    handle: Option<Handle>,
    threads: Vec<(String, JoinHandle<()>)>,
}

impl Host {
    /// Finds the local keyboards and mice and creates the passthrough
    /// device, without grabbing anything yet.
    pub fn new(hotkey: Hotkey, clients: Vec<PeerId>) -> anyhow::Result<Self> {
        let devices = discover().context("discovering input devices")?;
        if devices.is_empty() {
            bail!("this machine hosts clients but has no keyboard or mouse (is nioxd root?)");
        }
        for dev in &devices {
            info!(path = %dev.path().display(), name = dev.name(), "found input device");
        }
        let caps = union_capabilities(&devices);
        let km = HostKm::new(hotkey, clients, caps.clone())?;
        let local = VirtualDevice::create(&caps).context("creating the passthrough device")?;
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                km,
                local,
                out: Vec::new(),
                buf: Vec::new(),
            })),
            devices,
            handle: None,
            threads: Vec::new(),
        })
    }

    pub fn state(&self) -> Arc<Mutex<State>> {
        Arc::clone(&self.state)
    }

    /// Grabs every device on its own capture thread, and starts the timer
    /// thread that resends unacknowledged capabilities.
    pub fn start(&mut self, handle: Handle, stop: &Stop) -> anyhow::Result<()> {
        for dev in std::mem::take(&mut self.devices) {
            let name = format!("capture {}", dev.path().display());
            let (state, handle, stop) = (Arc::clone(&self.state), handle.clone(), stop.clone());
            let thread = thread::Builder::new()
                .name(name.clone())
                .spawn(move || capture(dev, &state, &handle, &stop))
                .with_context(|| format!("spawning {name}"))?;
            self.threads.push((name, thread));
        }

        let (state, timer_handle, stop) = (Arc::clone(&self.state), handle.clone(), stop.clone());
        let thread = thread::Builder::new()
            .name("km timers".into())
            .spawn(move || timers(&state, &timer_handle, &stop))
            .context("spawning the KM timer thread")?;
        self.threads.push(("km timers".into(), thread));
        self.handle = Some(handle);
        Ok(())
    }

    /// Joins every thread (the stop signal must already have triggered),
    /// then releases whatever is held on the focused client.
    pub fn shutdown(self) {
        for (name, thread) in self.threads {
            if thread.join().is_err() {
                error!("{name} panicked");
            }
        }
        if let Some(handle) = &self.handle {
            let mut s = lock(&self.state);
            s.step(handle, |km, out| {
                // Focus on the host is always available.
                let _ = km.set_focus(Target::Host, out);
            });
        }
    }
}

/// One device's capture thread: grab, then feed every frame to the KM
/// state until `stop` triggers or the device goes away.
fn capture(dev: InputDevice, state: &Mutex<State>, handle: &Handle, stop: &Stop) {
    let _stop_on_panic = stop.panic_guard();
    let path = dev.path().to_owned();
    let mut capture = match Capture::grab(dev, SETTLE, stop) {
        Ok(c) => c,
        Err(e) => {
            error!("{:#}", anyhow::Error::from(e));
            return;
        }
    };
    info!(path = %path.display(), name = capture.name(), "grabbed");
    let end = capture.run(stop, |frame| {
        lock(state).step(handle, |km, out| km.process_frame(frame, out));
    });
    match end {
        Ok(CaptureEnd::Stopped) => debug!(path = %path.display(), "released"),
        Ok(CaptureEnd::Gone) => warn!(path = %path.display(), "input device went away"),
        Err(e) => error!("{:#}", anyhow::Error::from(e)),
    }
}

/// Resends capabilities whose acknowledgment is overdue.
fn timers(state: &Mutex<State>, handle: &Handle, stop: &Stop) {
    let _stop_on_panic = stop.panic_guard();
    loop {
        let wait = lock(state).km.next_deadline().map_or(TICK, |due| {
            due.saturating_duration_since(Instant::now()).min(TICK)
        });
        if stop.wait_timeout(wait) {
            return;
        }
        lock(state).step(handle, |km, out| km.poll(Instant::now(), out));
    }
}
