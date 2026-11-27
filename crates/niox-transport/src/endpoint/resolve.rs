//! Resolving configured peer addresses. IP literals are filled in at once;
//! each hostname gets a thread that resolves it now and re-resolves it
//! periodically, so a slow or failing DNS lookup never stalls the driver.

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Weak};
use std::time::Duration;

use niox_config::PeerAddress;
use tracing::{debug, info};

use super::{SharedPeer, Timers, lock};

/// Retry interval while a hostname doesn't resolve.
const RETRY_FAILED: Duration = Duration::from_secs(5);

/// Dropping this stops the resolver thread (once any lookup in progress
/// returns; it's never joined).
pub(super) struct Stop(#[allow(dead_code)] mpsc::Sender<()>);

/// Fills in `peer`'s configured address: at once for an IP literal, or
/// from a new resolver thread for a hostname, whose [`Stop`] is returned.
/// The thread also exits once `peer` is dropped.
pub(super) fn start(
    v6: bool,
    peer: &Arc<SharedPeer>,
    name: &str,
    address: &PeerAddress,
    timers: Timers,
) -> io::Result<Option<Stop>> {
    let host = address.host();
    let port = address.port();
    if let Ok(ip) = host.parse::<IpAddr>() {
        *lock(&peer.configured) = Some(SocketAddr::new(ip, port));
        return Ok(None);
    }

    let (stop, stopped) = mpsc::channel::<()>();
    let peer = Arc::downgrade(peer);
    let name = name.to_owned();
    let interval = timers.resolve_interval;
    std::thread::Builder::new()
        .name(format!("niox-resolve-{name}"))
        .spawn(move || resolve_loop(v6, &peer, &name, &host, port, interval, &stopped))?;
    Ok(Some(Stop(stop)))
}

fn resolve_loop(
    v6: bool,
    peer: &Weak<SharedPeer>,
    name: &str,
    host: &str,
    port: u16,
    interval: Duration,
    stopped: &mpsc::Receiver<()>,
) {
    let mut last = None;
    loop {
        let resolved = resolve(host, port);
        let Some(peer) = peer.upgrade() else {
            return;
        };
        let addr = resolved.and_then(|addrs| {
            // Prefer IPv4: LAN hostnames often also resolve to link-local
            // IPv6 addresses that are useless without a scope.
            let v4 = addrs.iter().find(|a| a.is_ipv4());
            v4.or_else(|| addrs.iter().find(|_| v6)).copied()
        });
        {
            // Stopped while resolving: don't overwrite a newer address.
            // Checked under the lock, since the replacement address is set
            // under it after this thread's Stop is dropped; checking first
            // and locking after would let a stale write land last.
            let mut configured = lock(&peer.configured);
            if matches!(
                stopped.try_recv(),
                Ok(()) | Err(mpsc::TryRecvError::Disconnected)
            ) {
                return;
            }
            match addr {
                Some(addr) => {
                    if last != Some(addr) {
                        info!(peer = name, %host, %addr, "resolved peer address");
                    }
                    *configured = Some(addr);
                }
                None => debug!(peer = name, %host, "peer hostname did not resolve"),
            }
        }
        last = addr.or(last);
        drop(peer);

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
