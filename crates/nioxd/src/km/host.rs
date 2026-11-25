//! The host side: grab the local keyboards and mice (including ones
//! plugged in later), run the hotkey and focus logic, and send frames to
//! the focused client (or play them on this machine's own passthrough
//! device).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use niox_config::Hotkey;
use niox_ipc::{self as ipc, Publisher};
use niox_km::device::{
    Capture, CaptureEnd, InputDevice, Monitor, MonitorEvent, Stop, VirtualDevice, discover,
    union_capabilities,
};
use niox_km::{Capabilities, HostKm, HostOutput, Message, Target, Unavailable};
use niox_transport::{Event, Handle, PeerId};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
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
    /// Plays input on this machine while it has focus. Absent while there
    /// are no devices to capture.
    local: Option<VirtualDevice>,
    out: Vec<HostOutput<PeerId>>,
    buf: Vec<u8>,
    publisher: Publisher,
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
                    if let Some(local) = &mut self.local
                        && let Err(e) = local.play(&events)
                    {
                        warn!("{:#}", anyhow::Error::from(e));
                    }
                }
                HostOutput::Send { peer, message } => {
                    send(handle, &mut self.buf, peer, &message);
                }
                HostOutput::Focus(target) => {
                    let target = match target {
                        Target::Host => ipc::Target::Host,
                        Target::Client(peer) => ipc::Target::Client(handle.peer_name(peer).into()),
                    };
                    match &target {
                        ipc::Target::Host => info!(focus = "host", "focus moved"),
                        ipc::Target::Client(name) => info!(focus = name, "focus moved"),
                    }
                    self.publisher.update(|s| s.focus = Some(target));
                }
            }
        }
        self.out = out;
    }

    /// Grows the capabilities sent to clients, and the passthrough
    /// device's, to cover a newly captured device. They never shrink, so a
    /// device unplugged and plugged back in changes nothing. Returns
    /// `false` if the device can't be used.
    fn add_capabilities(&mut self, handle: &Handle, caps: &Capabilities) -> bool {
        let mut union = self.km.capabilities().clone();
        union.union_with(caps);
        if union == *self.km.capabilities() {
            return true;
        }
        let mut result = Ok(());
        self.step(handle, |km, out| {
            result = km.set_capabilities(union.clone(), Instant::now(), out);
        });
        if let Err(e) = result {
            error!(
                "{:#}",
                anyhow::Error::from(e).context("can't use a new input device")
            );
            return false;
        }
        // Destroying the old device releases anything held on it.
        match VirtualDevice::create(&union) {
            Ok(dev) => {
                self.local = Some(dev);
                self.km.local_device_replaced();
            }
            Err(e) => warn!(
                "{:#}",
                anyhow::Error::from(e).context("recreating the passthrough device")
            ),
        }
        info!("input capabilities grew; updating clients");
        true
    }

    pub fn is_ready(&self, peer: PeerId) -> bool {
        self.km.ready_clients().any(|p| p == peer)
    }
}

/// Moves focus on a control request. Errors are for the requester.
pub fn set_focus(
    state: &Mutex<State>,
    handle: &Handle,
    target: Target<PeerId>,
) -> Result<(), Unavailable> {
    let mut result = Ok(());
    lock(state).step(handle, |km, out| result = km.set_focus(target, out));
    result
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
    /// The hot-plug monitor (if it could be opened) and the devices found
    /// at startup, until [`Host::start`] hands them to the devices thread.
    startup: Option<(Option<Monitor>, Vec<InputDevice>)>,
    handle: Option<Handle>,
    threads: Vec<(String, JoinHandle<()>)>,
}

impl Host {
    /// Finds the local keyboards and mice and creates the passthrough
    /// device, without grabbing anything yet. Having none is fine: they're
    /// picked up when plugged in.
    pub fn new(hotkey: Hotkey, clients: Vec<PeerId>, publisher: Publisher) -> anyhow::Result<Self> {
        // Watch before scanning, so nothing plugged in between is missed.
        let monitor = match Monitor::open() {
            Ok(m) => Some(m),
            Err(e) => {
                error!("can't watch for input devices being plugged in: {e}");
                None
            }
        };
        let devices = discover().context("discovering input devices")?;
        if devices.is_empty() {
            warn!(
                "this machine hosts clients but has no keyboard or mouse yet \
                 (is nioxd root?); waiting for one to be plugged in"
            );
        }
        for dev in &devices {
            info!(path = %dev.path().display(), name = dev.name(), "found input device");
        }
        let caps = union_capabilities(&devices);
        let km = HostKm::new(hotkey, clients, caps.clone())?;
        let local = if caps.is_empty() {
            None
        } else {
            Some(VirtualDevice::create(&caps).context("creating the passthrough device")?)
        };
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                km,
                local,
                out: Vec::new(),
                buf: Vec::new(),
                publisher,
            })),
            startup: Some((monitor, devices)),
            handle: None,
            threads: Vec::new(),
        })
    }

    pub fn state(&self) -> Arc<Mutex<State>> {
        Arc::clone(&self.state)
    }

    /// Starts the devices thread, which grabs every device on its own
    /// capture thread and picks up devices plugged in later, and the timer
    /// thread that resends unacknowledged capabilities.
    pub fn start(&mut self, handle: Handle, stop: &Stop) -> anyhow::Result<()> {
        let (monitor, initial) = self.startup.take().unwrap_or_default();
        let devices = Devices {
            state: Arc::clone(&self.state),
            handle: handle.clone(),
            stop: stop.clone(),
            captures: Vec::new(),
        };
        let thread = thread::Builder::new()
            .name("km devices".into())
            .spawn(move || devices.run(monitor, initial))
            .context("spawning the KM devices thread")?;
        self.threads.push(("km devices".into(), thread));

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

/// The devices thread: starts a capture thread per keyboard or mouse,
/// at startup and whenever one is plugged in, and joins them all on stop.
#[derive(Debug)]
struct Devices {
    state: Arc<Mutex<State>>,
    handle: Handle,
    stop: Stop,
    captures: Vec<CaptureThread>,
}

#[derive(Debug)]
struct CaptureThread {
    /// The device node, until the device is removed. The node's name can
    /// then be reused by a new device while this thread is still
    /// finishing.
    path: Option<PathBuf>,
    thread: JoinHandle<()>,
}

impl Devices {
    fn run(mut self, monitor: Option<Monitor>, initial: Vec<InputDevice>) {
        let _stop_on_panic = self.stop.panic_guard();
        for dev in initial {
            self.add(dev);
        }
        if let Some(mut monitor) = monitor
            && let Err(e) = self.watch(&mut monitor)
        {
            error!("stopped watching for input devices being plugged in: {e:#}");
        }
        self.stop.wait();
        for c in self.captures {
            if c.thread.join().is_err() {
                error!("a capture thread panicked");
            }
        }
    }

    /// Handles plug and unplug events until `stop` triggers.
    fn watch(&mut self, monitor: &mut Monitor) -> anyhow::Result<()> {
        loop {
            let mut fds = [
                PollFd::new(monitor.fd(), PollFlags::POLLIN),
                PollFd::new(self.stop.fd(), PollFlags::POLLIN),
            ];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) | Err(Errno::EINTR) => {}
                Err(e) => return Err(anyhow::Error::from(e).context("waiting for uevents")),
            }
            if self.stop.is_triggered() {
                return Ok(());
            }
            while let Some(event) = monitor.read().context("reading uevents")? {
                match event {
                    MonitorEvent::Added(path) => self.plugged_in(&path),
                    MonitorEvent::Removed(path) => {
                        for c in &mut self.captures {
                            if c.path.as_ref() == Some(&path) {
                                c.path = None;
                            }
                        }
                    }
                    MonitorEvent::Overflow => {
                        warn!("missed some input device events; rescanning");
                        match discover() {
                            Ok(devices) => {
                                for dev in devices {
                                    if !self.is_captured(dev.path()) {
                                        self.add(dev);
                                    }
                                }
                            }
                            Err(e) => error!("{:#}", anyhow::Error::from(e)),
                        }
                    }
                }
            }
        }
    }

    fn plugged_in(&mut self, path: &Path) {
        // Already found by the startup scan (the monitor opens first).
        if self.is_captured(path) {
            return;
        }
        let dev = match InputDevice::open(path) {
            Ok(dev) => dev,
            Err(e) => {
                warn!(path = %path.display(), "can't open new input device: {e}");
                return;
            }
        };
        if !dev.is_keyboard_or_mouse() {
            debug!(path = %path.display(), name = dev.name(), "not a keyboard or mouse");
            return;
        }
        info!(path = %path.display(), name = dev.name(), "input device plugged in");
        self.add(dev);
    }

    /// Whether a capture thread is still running for `path`.
    fn is_captured(&mut self, path: &Path) -> bool {
        self.captures
            .iter()
            .any(|c| c.path.as_deref() == Some(path) && !c.thread.is_finished())
    }

    /// Makes sure clients can play what `dev` produces, then starts
    /// capturing it.
    fn add(&mut self, dev: InputDevice) {
        if !lock(&self.state).add_capabilities(&self.handle, &dev.capabilities()) {
            return;
        }
        let path = dev.path().to_owned();
        let name = format!(
            "capture {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let (state, handle, stop) = (
            Arc::clone(&self.state),
            self.handle.clone(),
            self.stop.clone(),
        );
        match thread::Builder::new()
            .name(name)
            .spawn(move || capture(dev, &state, &handle, &stop))
        {
            Ok(thread) => {
                // Forget threads that have finished.
                self.captures.retain(|c| !c.thread.is_finished());
                self.captures.push(CaptureThread {
                    path: Some(path),
                    thread,
                });
            }
            Err(e) => error!(path = %path.display(), "spawning a capture thread: {e}"),
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
