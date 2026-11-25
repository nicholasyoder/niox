//! Resolving configured peer addresses. IP literals are filled in at once;
//! each hostname gets a thread that resolves it now and re-resolves it
//! periodically, so a slow or failing DNS lookup never stalls the driver.

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Weak};
use std::time::Duration;

use tracing::{debug, info};

use super::{PeerSpec, Shared, Timers, lock};

/// Retry interval while a hostname doesn't resolve.
const RETRY_FAILED: Duration = Duration::from_secs(5);

/// Dropping this stops the resolver threads (once any lookup in progress
/// returns; they're never joined).
pub(super) struct Stop(#[allow(dead_code)] Vec<mpsc::Sender<()>>);

pub(super) fn start(
    shared: &Arc<Shared>,
    peers: &[PeerSpec],
    timers: Timers,
) -> io::Result<Option<Stop>> {
    let mut stops = Vec::new();
    for (index, spec) in peers.iter().enumerate() {
        let host = spec.address.host();
        let port = spec.address.port();
        if let Ok(ip) = host.parse::<IpAddr>() {
            *lock(&shared.peers[index].configured) = Some(SocketAddr::new(ip, port));
            continue;
        }

        let (stop, stopped) = mpsc::channel::<()>();
        let shared = Arc::downgrade(shared);
        let name = spec.name.clone();
        let interval = timers.resolve_interval;
        std::thread::Builder::new()
            .name(format!("niox-resolve-{name}"))
            .spawn(move || resolve_loop(&shared, index, &name, &host, port, interval, &stopped))?;
        stops.push(stop);
    }
    Ok((!stops.is_empty()).then_some(Stop(stops)))
}

fn resolve_loop(
    shared: &Weak<Shared>,
    index: usize,
    name: &str,
    host: &str,
    port: u16,
    interval: Duration,
    stopped: &mpsc::Receiver<()>,
) {
    let mut last = None;
    loop {
        let resolved = resolve(host, port);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let v6 = shared.v6;
        let addr = resolved.and_then(|addrs| {
            // Prefer IPv4: LAN hostnames often also resolve to link-local
            // IPv6 addresses that are useless without a scope.
            let v4 = addrs.iter().find(|a| a.is_ipv4());
            v4.or_else(|| addrs.iter().find(|_| v6)).copied()
        });
        match addr {
            Some(addr) => {
                if last != Some(addr) {
                    info!(peer = name, %host, %addr, "resolved peer address");
                }
                *lock(&shared.peers[index].configured) = Some(addr);
            }
            None => debug!(peer = name, %host, "peer hostname did not resolve"),
        }
        last = addr.or(last);
        drop(shared);

        let wait = if addr.is_some() {
            interval
        } else {
            interval.min(RETRY_FAILED)
        };
        match stopped.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn resolve(host: &str, port: u16) -> Option<Vec<SocketAddr>> {
    (host, port)
        .to_socket_addrs()
        .ok()
        .map(Iterator::collect)
        .filter(|addrs: &Vec<_>| !addrs.is_empty())
}
