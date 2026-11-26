//! niox-session: the optional, unprivileged half of niox, run per user
//! under `systemctl --user`. See ARCHITECTURE.md.
//!
//! It does what needs a logged-in user's session rather than root (audio,
//! later), and learns everything it needs from nioxd over the control
//! socket: it subscribes to focus and peer status and keeps a [`View`] of
//! them, logging each change. It never reads nioxd's config file.
//!
//! It also asks nioxd for the audio channel, which nioxd grants to one
//! niox-session at a time (the active seat user's), and while it holds
//! it runs audio in the user's PipeWire ([`audio`]): on a client, a niox
//! sink that's the default output and goes to its host; on a host, a
//! player for each client sending to it. The `[audio]` settings come
//! from nioxd too, and a reload applies them.
//!
//! nioxd never depends on it. When nioxd isn't there (stopped,
//! restarting, or not installed yet), niox-session waits and reconnects
//! with backoff; whatever changed while it was away is reported once the
//! new snapshot arrives.

mod audio;
mod view;

use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use clap::Parser;
use niox_ipc::protocol::PROTOCOL_VERSION;
use niox_ipc::{AudioChannel, Client, ClientError, Event};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use crate::audio::Audio;
use crate::view::{Change, View};

/// The first wait before reconnecting to nioxd; doubles up to
/// [`RETRY_MAX`].
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(version, about = "niox user-session service")]
struct Args {
    /// nioxd's control socket.
    #[arg(long, value_name = "PATH", default_value = niox_ipc::DEFAULT_SOCKET)]
    socket: PathBuf,
}

fn main() {
    init_logging();
    let args = Args::parse();
    info!(socket = %args.socket.display(), "starting");
    let audio = Audio::start();
    stop_cleanly_on_signals(&audio);
    follow(&args.socket, &audio)
}

/// On SIGTERM (`systemctl --user stop`) or SIGINT, removes the niox sink
/// and gives the default output back before exiting. (After a crash or
/// `kill -9` the session manager falls back to a real output anyway.)
fn stop_cleanly_on_signals(audio: &Audio) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGINT, SIGTERM]) {
        Ok(signals) => signals,
        Err(e) => {
            warn!("can't handle signals ({e}); a stop won't give the default output back");
            return;
        }
    };
    let engine = audio.engine();
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            info!(signal, "stopping");
            if let Some(engine) = engine {
                engine.shutdown();
            }
            std::process::exit(0);
        }
    });
}

/// Follows nioxd's status for as long as the process runs, reconnecting
/// whenever the connection ends.
fn follow(socket: &Path, audio: &Audio) -> ! {
    let mut view = View::default();
    let mut retry = RETRY_MIN;
    // The last connection failure, so a nioxd that stays away is logged
    // once rather than on every attempt.
    let mut failing: Option<String> = None;
    loop {
        let result = session(socket, &mut view, audio, &mut failing, &mut |change| {
            info!("{change}")
        });
        // Without nioxd there's no channel, so no audio to send.
        audio.set_channel(None, &view);
        match result {
            Ok(()) => {
                info!("nioxd closed the connection (stopped or restarting); reconnecting");
                retry = RETRY_MIN;
            }
            Err(e) => {
                let hint = hint(&e);
                let message = format!("{:#}", anyhow::Error::from(e));
                if failing.as_deref() != Some(&message) {
                    warn!("{message}; retrying until it's available{hint}");
                } else {
                    debug!("{message}");
                }
                failing = Some(message);
            }
        }
        thread::sleep(retry);
        retry = (retry * 2).min(RETRY_MAX);
    }
}

/// One connection to nioxd: ask for the audio channel and subscribe, then
/// apply events until it ends, passing each change in `view` (and each
/// grant or loss of the audio channel) to `report`, and keeping `audio`
/// in line with them. Returns `Ok` when nioxd closes the connection.
fn session(
    socket: &Path,
    view: &mut View,
    audio: &Audio,
    failing: &mut Option<String>,
    report: &mut dyn FnMut(&Change),
) -> Result<(), ClientError> {
    let mut client = Client::connect(socket)?;
    client.request_audio()?;
    let (status, mut events) = client.subscribe()?;
    info!(
        nioxd = %status.version,
        peers = status.peers.len(),
        "connected to nioxd"
    );
    if failing.take().is_some() {
        debug!("nioxd is back");
    }
    if status.protocol != PROTOCOL_VERSION {
        warn!(
            nioxd = status.protocol,
            ours = PROTOCOL_VERSION,
            "nioxd speaks a different control protocol version; update niox-session and nioxd together"
        );
    }
    for change in view.replace(View::from_status(&status)) {
        report(&change);
    }
    audio.update(view);
    while let Some(event) = events.next_event()? {
        if let Event::Audio { granted } = event {
            let channel: Option<AudioChannel> = events.take_audio_channel();
            if granted && channel.is_none() {
                warn!("nioxd granted the audio channel but it didn't arrive");
            }
            report(&Change::Audio(granted));
            audio.set_channel(channel, view);
            continue;
        }
        for change in view.apply(&event) {
            report(&change);
        }
        audio.update(view);
    }
    Ok(())
}

/// A next step for a connection failure with an obvious cause.
fn hint(e: &ClientError) -> String {
    match e {
        ClientError::Connect { source, .. } if source.kind() == io::ErrorKind::PermissionDenied => {
            format!(
                " (using nioxd's socket takes membership in the {:?} group: \
                 `sudo usermod -aG {} $USER`, then log in again)",
                niox_ipc::GROUP,
                niox_ipc::GROUP
            )
        }
        _ => String::new(),
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
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .init();
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use niox_ipc::{Handler, PeerStatus, Publisher, Role, Server, Status, Target};

    use super::*;

    struct NoControl;

    impl Handler for NoControl {
        fn focus(&self, _: &Target) -> Result<(), String> {
            Err("not in this test".into())
        }

        fn reload(&self) -> Result<Vec<String>, String> {
            Err("not in this test".into())
        }

        fn audio_channel(&self, _: Option<AudioChannel>) {}

        fn active_uid(&self) -> Option<u32> {
            None
        }
    }

    fn status(focus: Target, desk_connected: bool) -> Status {
        Status {
            version: "0.1.0".into(),
            protocol: PROTOCOL_VERSION,
            public_key: "k".into(),
            port: 4777,
            focus: Some(focus),
            peers: vec![PeerStatus {
                name: "desk".into(),
                address: "desk.lan".into(),
                role: Role::Client,
                connected: desk_connected,
                in_focus_cycle: desk_connected,
            }],
            audio: Default::default(),
        }
    }

    #[test]
    fn follows_nioxd_until_it_goes_away() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("niox.sock");
        let publisher = Publisher::new(status(Target::Host, false));
        let server = Server::start(&path, None, publisher.clone(), NoControl).unwrap();

        let (tx, rx) = mpsc::channel();
        let session = {
            let path = path.clone();
            thread::spawn(move || {
                let mut view = View::default();
                let result = session(&path, &mut view, &Audio::none(), &mut None, &mut |c| {
                    tx.send(c.clone()).unwrap();
                });
                (result.is_ok(), view)
            })
        };
        let next = || rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // The snapshot, against an empty view.
        assert_eq!(next(), Change::Focus(Some(Target::Host)));
        assert!(matches!(next(), Change::Added(p) if p.name == "desk" && !p.connected));
        // The only one asking, so it gets the audio channel.
        assert_eq!(next(), Change::Audio(true));

        publisher.update(|s| *s = status(Target::Client("desk".into()), true));
        assert_eq!(next(), Change::Focus(Some(Target::Client("desk".into()))));
        assert_eq!(next(), Change::Connected("desk".into()));

        // nioxd stopping ends the session cleanly, keeping the last view.
        drop(server);
        let (ok, view) = session.join().unwrap();
        assert!(ok);
        assert_eq!(view.focus, Some(Target::Client("desk".into())));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_missing_socket_is_a_connect_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = session(
            &dir.path().join("niox.sock"),
            &mut View::default(),
            &Audio::none(),
            &mut None,
            &mut |_| {},
        );
        assert!(matches!(result, Err(ClientError::Connect { .. })));
    }
}
