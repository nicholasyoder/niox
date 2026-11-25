//! Grabs every local keyboard and mouse and replays them through one
//! uinput device, so input keeps working normally while niox owns it.
//! Needs root (or access to `/dev/input/event*` and `/dev/uinput`).
//!
//! ```text
//! sudo km-passthrough                      # until Ctrl-C / SIGTERM
//! sudo km-passthrough --exit-after 30      # stop on its own
//! sudo km-passthrough --panic capture --panic-after 10
//! sudo km-passthrough --panic main --panic-after 10
//! ```
//!
//! `--panic` forces a panic in a capture thread (while it holds its grab)
//! or in the playback thread, to check that nothing stays grabbed and no
//! virtual device is left behind.

use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use clap::{Parser, ValueEnum};
use niox_km::InputEvent;
use niox_km::device::{Capture, CaptureEnd, Stop, VirtualDevice, discover, union_capabilities};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
struct Args {
    /// How long to wait for held keys to be released before grabbing.
    #[arg(long, value_name = "MS", default_value_t = 2000)]
    settle_ms: u64,
    /// Stop after this many seconds.
    #[arg(long, value_name = "SECS")]
    exit_after: Option<u64>,
    /// Force a panic in this thread.
    #[arg(long, requires = "panic_after")]
    panic: Option<PanicIn>,
    /// When to force the panic, in seconds after startup. A capture thread
    /// panics on its first frame after this.
    #[arg(long, value_name = "SECS", requires = "panic")]
    panic_after: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PanicIn {
    /// The first device's capture thread.
    Capture,
    /// The playback (main) thread.
    Main,
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let start = Instant::now();
    let stop = Stop::new().context("creating the stop signal")?;
    // Whatever ends this function, the capture threads stop too.
    let _stop_on_exit = stop.guard();

    let mut signals =
        Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT]).context("installing signal handlers")?;
    {
        let stop = stop.clone();
        thread::spawn(move || {
            if let Some(sig) = signals.forever().next() {
                info!(signal = sig, "stopping");
                stop.trigger();
            }
        });
    }

    let devices = discover().context("discovering input devices")?;
    if devices.is_empty() {
        bail!("no keyboards or mice found (is this running as root?)");
    }
    for dev in &devices {
        info!(path = %dev.path().display(), name = dev.name(), "found");
    }
    let caps = union_capabilities(&devices);
    let mut virt = VirtualDevice::create(&caps).context("creating the virtual device")?;
    info!("virtual device created");

    let (tx, rx) = mpsc::channel::<Vec<InputEvent>>();
    let settle = Duration::from_millis(args.settle_ms);
    let panic_at = args.panic_after.map(|s| start + Duration::from_secs(s));
    let mut workers = Vec::new();
    for (i, dev) in devices.into_iter().enumerate() {
        let stop = stop.clone();
        let tx = tx.clone();
        let panic_at = panic_at.filter(|_| i == 0 && args.panic == Some(PanicIn::Capture));
        let name = format!("capture {}", dev.path().display());
        let worker = thread::Builder::new()
            .name(name.clone())
            .spawn(move || -> anyhow::Result<()> {
                let _stop_on_panic = stop.panic_guard();
                let mut capture = Capture::grab(dev, settle, &stop)?;
                info!(path = %capture.path().display(), name = capture.name(), "grabbed");
                let end = capture.run(&stop, |frame| {
                    if panic_at.is_some_and(|t| Instant::now() >= t) {
                        panic!("forced panic in a capture thread");
                    }
                    // The receiver is gone only when we're stopping anyway.
                    let _ = tx.send(frame.to_vec());
                })?;
                match end {
                    CaptureEnd::Stopped => debug!(path = %capture.path().display(), "stopped"),
                    CaptureEnd::Gone => warn!(path = %capture.path().display(), "device went away"),
                }
                Ok(())
            })
            .with_context(|| format!("spawning {name}"))?;
        workers.push((name, worker));
    }
    drop(tx);

    let exit_at = args.exit_after.map(|s| start + Duration::from_secs(s));
    let main_panic_at = panic_at.filter(|_| args.panic == Some(PanicIn::Main));
    let mut frames = 0u64;
    let result = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                frames += 1;
                debug!(?frame, "frame");
                if let Err(e) = virt.play(&frame) {
                    break Err(e).context("playing a frame");
                }
            }
            // Every capture thread has ended.
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if main_panic_at.is_some_and(|t| now >= t) {
            panic!("forced panic in the playback thread");
        }
        if exit_at.is_some_and(|t| now >= t) {
            info!("time's up; stopping");
            stop.trigger();
        }
    };

    stop.trigger();
    for (name, worker) in workers {
        match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!("{name}: {e:#}"),
            Err(_) => error!("{name} panicked"),
        }
    }
    drop(virt);
    info!(
        frames,
        "released every device and removed the virtual device"
    );
    result
}
