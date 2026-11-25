//! nioxd: niox's privileged system service. See ARCHITECTURE.md.
//!
//! It loads its config (generating the identity keypair on first run),
//! joins the transport, and runs keyboard/mouse for every configured peer:
//! as the host for peers with role `client` (grabbing the local keyboards
//! and mice and sending them to the focused client), and as a client of
//! peers with role `host` (playing what they send into a virtual device).
//! It needs no user session, so it works at the login screen.
//!
//! It also serves the local control socket (`/run/niox/niox.sock`), which
//! reports focus and peer status and takes control requests from nioxctl.

mod km;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context as _, bail};
use clap::Parser;
use niox_config::{Config, Role};
use niox_ipc::protocol::PROTOCOL_VERSION;
use niox_ipc::{self as ipc, PeerStatus, Publisher, Server, Status};
use niox_km::device::Stop;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

#[derive(Debug, Parser)]
#[command(version, about = "niox privileged system service")]
struct Args {
    /// Config file to use instead of /etc/niox/niox.toml, e.g. to run
    /// unprivileged against a local file during development.
    #[arg(long, value_name = "PATH", default_value = niox_config::DEFAULT_PATH)]
    config: PathBuf,

    /// Control socket to listen on instead of /run/niox/niox.sock.
    #[arg(long, value_name = "PATH", default_value = niox_ipc::DEFAULT_SOCKET)]
    socket: PathBuf,
}

fn main() -> ExitCode {
    init_logging();
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let loaded = niox_config::load(&args.config).context("loading config")?;
    let config = &loaded.config;
    if loaded.created_identity {
        info!(path = %args.config.display(), "generated a new identity keypair");
    }
    info!(
        path = %args.config.display(),
        public_key = %config.identity.public_key,
        port = config.transport.port,
        peers = config.peers.len(),
        "config loaded"
    );
    for peer in &config.peers {
        info!(
            name = %peer.name,
            address = %peer.address,
            role = peer.role.as_str(),
            public_key = %peer.public_key,
            "peer"
        );
    }
    if config.peers.is_empty() {
        warn!("no peers configured; nothing to do until some are added");
    }

    // One shutdown signal for everything: a signal, or any worker thread
    // panicking, triggers it.
    let stop = Stop::new().context("creating the stop signal")?;
    let _stop_on_exit = stop.guard();
    let signaled = Arc::new(AtomicBool::new(false));
    let mut signals = Signals::new([SIGINT, SIGTERM]).context("installing signal handlers")?;
    {
        let stop = stop.clone();
        let signaled = Arc::clone(&signaled);
        thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                if let Some(sig) = signals.forever().next() {
                    info!(signal = sig, "stopping");
                    signaled.store(true, Ordering::SeqCst);
                    stop.trigger();
                }
            })
            .context("spawning the signal thread")?;
    }

    let publisher = Publisher::new(initial_status(config));
    let km = km::Km::start(config, &stop, &publisher)?;
    // KM never depends on the control socket: if it can't be served,
    // say so and carry on.
    let control = match Server::start(&args.socket, socket_group(), publisher, km.control()) {
        Ok(server) => {
            info!(path = %args.socket.display(), "control socket listening");
            Some(server)
        }
        Err(e) => {
            error!(
                "{:#}",
                anyhow::Error::from(e).context("control socket unavailable (nioxctl won't work)")
            );
            None
        }
    };
    info!("running");
    stop.wait();
    drop(control);
    km.shutdown();

    if !signaled.load(Ordering::SeqCst) {
        bail!("a worker thread failed; exiting");
    }
    info!("stopped");
    Ok(())
}

/// The status the control socket reports before anything has happened.
fn initial_status(config: &Config) -> Status {
    let role = |r| match r {
        Role::Host => ipc::Role::Host,
        Role::Client => ipc::Role::Client,
    };
    Status {
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL_VERSION,
        public_key: config.identity.public_key.to_string(),
        port: config.transport.port,
        focus: config
            .peers
            .iter()
            .any(|p| p.role == Role::Client)
            .then_some(ipc::Target::Host),
        peers: config
            .peers
            .iter()
            .map(|p| PeerStatus {
                name: p.name.clone(),
                address: p.address.to_string(),
                role: role(p.role),
                connected: false,
                in_focus_cycle: false,
            })
            .collect(),
    }
}

/// The group the control socket is shared with: `niox` if it exists,
/// otherwise nobody but root.
fn socket_group() -> Option<u32> {
    match niox_ipc::lookup_group(niox_ipc::GROUP) {
        Ok(Some(gid)) => Some(gid),
        Ok(None) => {
            warn!(
                "no {:?} group; the control socket is limited to its owner (root under systemd)",
                niox_ipc::GROUP
            );
            None
        }
        Err(e) => {
            warn!(
                "looking up the {:?} group: {e}; the control socket is limited to its owner (root under systemd)",
                niox_ipc::GROUP
            );
            None
        }
    }
}

/// Logs to the journal when running under systemd (which sets
/// `JOURNAL_STREAM`), otherwise to stderr. `RUST_LOG` overrides the
/// default `info` level.
fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);

    if std::env::var_os("JOURNAL_STREAM").is_some()
        && let Ok(journald) = tracing_journald::layer()
    {
        registry.with(journald).init();
        return;
    }
    registry
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}
