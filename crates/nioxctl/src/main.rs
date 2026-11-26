//! nioxctl: control nioxd from the command line, over its local socket.
//!
//! Using the socket takes membership in the `niox` group (or root).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use niox_ipc::{Client, ClientError, Event, PeerStatus, Role, Status, Target};

#[derive(Debug, Parser)]
#[command(version, about = "Control nioxd")]
struct Args {
    /// nioxd's control socket.
    #[arg(long, global = true, value_name = "PATH", default_value = niox_ipc::DEFAULT_SOCKET)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show where focus is and each peer's state.
    Status {
        /// Print nioxd's status as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print this machine's public key, for adding it to a peer's config.
    Pubkey,
    /// Move keyboard/mouse focus to a client, or back to this machine.
    Focus {
        /// A client's name from the config, or `host` for this machine.
        target: String,
    },
    /// Have nioxd re-read its config and apply it (like `systemctl reload
    /// nioxd`), and print what changed.
    Reload,
    /// Print focus and peer changes as they happen, until interrupted.
    Watch {
        /// One JSON object per line: the status, then each event.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        // A closed stdout (e.g. piped into `head`) isn't worth reporting.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "nioxctl: {e:#}");
            if let Some(hint) = hint(&e, &args.socket) {
                let _ = writeln!(io::stderr(), "{hint}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> anyhow::Result<()> {
    let mut client = Client::connect(&args.socket)?;
    let mut out = io::stdout().lock();
    match &args.command {
        Command::Status { json } => {
            let status = client.status()?;
            if *json {
                serde_json::to_writer_pretty(&mut out, &status)?;
                writeln!(out)?;
            } else {
                print_status(&mut out, &status)?;
            }
        }
        Command::Pubkey => writeln!(out, "{}", client.status()?.public_key)?,
        Command::Focus { target } => {
            let target = if target == "host" {
                Target::Host
            } else {
                Target::Client(target.clone())
            };
            client.focus(target)?;
        }
        Command::Reload => {
            let changes = client.reload()?;
            if changes.is_empty() {
                writeln!(out, "reloaded; nothing changed")?;
            }
            for change in changes {
                writeln!(out, "{change}")?;
            }
        }
        Command::Watch { json } => {
            let (status, events) = client.subscribe()?;
            if *json {
                serde_json::to_writer(&mut out, &status)?;
                writeln!(out)?;
            } else {
                print_status(&mut out, &status)?;
            }
            out.flush()?;
            for event in events {
                let event = event?;
                if *json {
                    serde_json::to_writer(&mut out, &event)?;
                    writeln!(out)?;
                } else {
                    print_event(&mut out, &event)?;
                }
                out.flush()?;
            }
            anyhow::bail!("nioxd closed the connection (stopped?)");
        }
    }
    Ok(())
}

fn print_status(out: &mut impl Write, status: &Status) -> io::Result<()> {
    writeln!(out, "nioxd {}, UDP port {}", status.version, status.port)?;
    writeln!(out, "public key: {}", status.public_key)?;
    match &status.focus {
        Some(focus) => writeln!(out, "focus: {}", focus_name(focus))?,
        None => writeln!(out, "focus: - (this machine hosts no clients)")?,
    }
    let audio = &status.audio;
    if audio.enabled {
        writeln!(
            out,
            "audio: {}, latency {}ms (applied by niox-session)",
            audio.mode.as_str(),
            audio.latency_ms
        )?;
    } else {
        writeln!(out, "audio: disabled")?;
    }
    if status.peers.is_empty() {
        return writeln!(out, "peers: none configured");
    }
    writeln!(out, "peers:")?;
    let width = status.peers.iter().map(|p| p.name.len()).max().unwrap_or(0);
    let addr_width = status
        .peers
        .iter()
        .map(|p| p.address.len())
        .max()
        .unwrap_or(0);
    for p in &status.peers {
        writeln!(
            out,
            "  {:width$}  {:6}  {:addr_width$}  {}",
            p.name,
            p.role.as_str(),
            p.address,
            peer_state(p),
        )?;
    }
    Ok(())
}

fn print_event(out: &mut impl Write, event: &Event) -> io::Result<()> {
    match event {
        Event::Focus { focus } => writeln!(out, "focus: {}", focus_name(focus)),
        Event::Peer(p) => writeln!(
            out,
            "peer {} ({}): {}",
            p.name,
            p.role.as_str(),
            peer_state(p)
        ),
        Event::Status(status) => {
            writeln!(out, "status changed:")?;
            print_status(out, status)
        }
        // Only sent to a connection that asked for audio, which watch
        // doesn't.
        Event::Audio { granted } => writeln!(out, "audio channel granted: {granted}"),
    }
}

fn focus_name(target: &Target) -> &str {
    match target {
        Target::Host => "host (this machine)",
        Target::Client(name) => name,
    }
}

fn peer_state(p: &PeerStatus) -> &'static str {
    match (p.connected, p.role, p.in_focus_cycle) {
        (false, _, _) => "disconnected",
        (true, Role::Client, true) => "connected, in focus cycle",
        (true, Role::Client, false) => "connected, not ready yet",
        (true, Role::Host, _) => "connected",
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        let kind = cause
            .downcast_ref::<io::Error>()
            .map(io::Error::kind)
            .or_else(|| cause.downcast_ref::<serde_json::Error>()?.io_error_kind());
        kind == Some(io::ErrorKind::BrokenPipe)
    })
}

/// A next step for errors with an obvious cause.
fn hint(e: &anyhow::Error, socket: &Path) -> Option<String> {
    let ClientError::Connect { source, .. } = e.downcast_ref::<ClientError>()? else {
        return None;
    };
    match source.kind() {
        io::ErrorKind::PermissionDenied => Some(format!(
            "using {} takes membership in the {:?} group (or root): \
             `sudo usermod -aG {} $USER`, then log in again",
            socket.display(),
            niox_ipc::GROUP,
            niox_ipc::GROUP,
        )),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            Some("is nioxd running? (`systemctl status nioxd`)".into())
        }
        _ => None,
    }
}
