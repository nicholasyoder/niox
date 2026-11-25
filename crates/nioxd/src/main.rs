//! nioxd: niox's privileged system service. See ARCHITECTURE.md.
//!
//! So far it only loads and validates its config (generating the identity
//! keypair on first run), logs a summary, and exits.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

#[derive(Debug, Parser)]
#[command(version, about = "niox privileged system service")]
struct Args {
    /// Config file to use instead of /etc/niox/niox.toml, e.g. to run
    /// unprivileged against a local file during development.
    #[arg(long, value_name = "PATH", default_value = niox_config::DEFAULT_PATH)]
    config: PathBuf,
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
    Ok(())
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
