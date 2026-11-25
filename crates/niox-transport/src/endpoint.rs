//! The socket half: one UDP socket per nioxd, serving every configured
//! peer, with handshake retransmission, keepalives, liveness detection, and
//! automatic re-handshakes.
//!
//! An [`Endpoint`] owns a driver thread that blocks on the socket, answers
//! and retries handshakes, sends keepalives, and decrypts every datagram
//! that arrives, passing the results to a handler on that same thread
//! (receive → decrypt → handler, with no channel hop in between). Sending
//! needs no help from the driver: any thread holding a [`Handle`] encrypts
//! and sends directly on the shared socket.
//!
//! # Session lifecycle
//!
//! Every peer is either *up* (it has an established session) or *down*.
//! While down, this side sends handshake inits with backoff
//! ([`Timers::retry_initial`] doubling to [`Timers::retry_max`]),
//! retransmitting the same init so a peer that already answered it can
//! resend its cached response, and starting a fresh one after
//! [`Timers::init_lifetime`]. Both sides do this, so whichever comes back
//! first reconnects the pair.
//!
//! - The initiator is up as soon as the response verifies, and immediately
//!   sends a keepalive to confirm the session.
//! - The responder holds its new session unconfirmed until the first
//!   authenticated datagram arrives on it, so a forged-but-replayed init or
//!   a lost response can't replace a working session.
//! - If both sides initiate at once, the side with the lower public key
//!   keeps its own init and ignores the other's; the other side answers it.
//! - Up, each side sends a keepalive whenever it hasn't sent anything for
//!   [`Timers::keepalive`]. A peer that sends nothing authenticated for
//!   [`Timers::dead_after`] goes down, and handshaking starts again.
//! - A peer that restarts handshakes immediately on startup. The other
//!   side's existing session is replaced once the new one is confirmed,
//!   reported as [`Event::Down`] followed by [`Event::Up`], since the
//!   peer's own state is gone.
//!
//! # Addresses
//!
//! Each peer's configured address is where inits are sent. Hostnames are
//! resolved off the driver thread and re-resolved periodically. Once a
//! datagram from the peer authenticates, replies go to its source address
//! instead (so a peer whose DHCP lease changed keeps working), and while
//! down, inits go to both. Datagrams are matched to sessions by receiver
//! index and authenticated by their AEAD tag. Handshake inits, which carry
//! no index, are tried first against the peer whose address they came from
//! and then against every other peer's key. Anything that doesn't
//! authenticate is dropped without a reply.

mod driver;
mod resolve;

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use niox_config::{Config, PeerAddress, PrivateKey, PublicKey};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::warn;

use crate::{Error, MAX_DATAGRAM, Payload, Session, Subsystem};

/// Identifies one configured peer of an [`Endpoint`]: its position in
/// [`EndpointConfig::peers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(u32);

impl PeerId {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The peer at `index` in [`EndpointConfig::peers`] (for
    /// [`EndpointConfig::from_config`], its index in the config's peers).
    pub fn new(index: usize) -> Self {
        // Peer lists are tiny; Endpoint::start caps them.
        Self(u32::try_from(index).unwrap_or(u32::MAX))
    }
}

/// A peer the endpoint talks to.
#[derive(Debug, Clone)]
pub struct PeerSpec {
    pub name: String,
    pub public_key: PublicKey,
    pub address: PeerAddress,
}

/// The endpoint's timing. The defaults suit a LAN: a peer that vanishes is
/// noticed within [`dead_after`](Self::dead_after), and once it's
/// reachable again the pair reconnects within about
/// [`retry_max`](Self::retry_max).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timers {
    /// Send a keepalive after this long without sending anything else.
    pub keepalive: Duration,
    /// Declare a peer down after this long without receiving anything
    /// authenticated from it.
    pub dead_after: Duration,
    /// First handshake retransmission delay; doubles on each retry.
    pub retry_initial: Duration,
    /// Cap on the retransmission delay.
    pub retry_max: Duration,
    /// Retransmit the same init for this long, then start a fresh one.
    pub init_lifetime: Duration,
    /// How often to re-resolve a peer configured by hostname.
    pub resolve_interval: Duration,
}

impl Default for Timers {
    fn default() -> Self {
        Self {
            keepalive: Duration::from_secs(1),
            dead_after: Duration::from_millis(3500),
            retry_initial: Duration::from_millis(250),
            retry_max: Duration::from_secs(2),
            init_lifetime: Duration::from_secs(15),
            resolve_interval: Duration::from_secs(30),
        }
    }
}

/// Everything an [`Endpoint`] needs besides its socket.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub private_key: PrivateKey,
    pub peers: Vec<PeerSpec>,
    pub timers: Timers,
}

impl EndpointConfig {
    /// The identity and peers from a loaded niox config, with default
    /// timers.
    pub fn from_config(config: &Config) -> Self {
        Self {
            private_key: config.identity.private_key.clone(),
            peers: config
                .peers
                .iter()
                .map(|p| PeerSpec {
                    name: p.name.clone(),
                    public_key: p.public_key,
                    address: p.address.clone(),
                })
                .collect(),
            timers: Timers::default(),
        }
    }
}

/// What the driver thread reports to the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// A new session with the peer is established. Anything the peer knew
    /// about this side from an earlier session should be assumed lost.
    Up(PeerId),
    /// The session with the peer is gone (it stopped responding, or it is
    /// being replaced by a new one). Always follows an `Up`.
    Down(PeerId),
    /// A subsystem message from the peer. Datagrams can arrive lost,
    /// duplicated-and-dropped, or reordered; ordering is not restored.
    Data {
        peer: PeerId,
        subsystem: Subsystem,
        body: &'a [u8],
    },
}

/// Why [`Handle::send`] failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SendError {
    /// No such peer, or it isn't up.
    #[error("peer is not connected")]
    NotConnected,
    #[error("encrypting datagram")]
    Seal(#[source] Error),
    /// The socket refused the datagram (e.g. the network is down). The
    /// peer will be declared down if this persists.
    #[error("sending datagram")]
    Io(#[source] io::Error),
}

/// A running transport endpoint. Dropping it stops the driver thread;
/// [`Handle`]s that outlive it can no longer send.
pub struct Endpoint {
    handle: Handle,
    driver: Option<JoinHandle<()>>,
    /// Dropping this tells the resolver threads to exit.
    _resolvers: Option<resolve::Stop>,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    /// Binds the transport's UDP socket on every interface: dual-stack
    /// IPv6 where available (IPv4 peers arrive as mapped addresses), IPv4
    /// only otherwise. Port 0 picks a free port.
    pub fn bind(port: u16) -> io::Result<UdpSocket> {
        let v6 = || -> io::Result<UdpSocket> {
            let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_only_v6(false)?;
            socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
            Ok(socket.into())
        };
        v6().or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)))
    }

    /// Starts the driver thread on `socket` (normally from
    /// [`bind`](Self::bind)). It immediately begins handshaking with every
    /// peer.
    ///
    /// `handler` runs on the driver thread for every [`Event`]. It should
    /// return quickly: while it runs, nothing is received and no timers
    /// fire. It may send through the [`Handle`] it's given.
    pub fn start<F>(socket: UdpSocket, config: EndpointConfig, handler: F) -> io::Result<Self>
    where
        F: FnMut(&Handle, Event<'_>) + Send + 'static,
    {
        if config.peers.len() > u16::MAX.into() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many peers",
            ));
        }
        let local_addr = socket.local_addr()?;
        let shared = Arc::new(Shared {
            v6: local_addr.is_ipv6(),
            local_addr,
            socket,
            stopped: AtomicBool::new(false),
            epoch: Instant::now(),
            peers: config
                .peers
                .iter()
                .map(|spec| SharedPeer {
                    name: spec.name.clone(),
                    up: AtomicBool::new(false),
                    route: Mutex::new(Route::default()),
                    configured: Mutex::new(None),
                    last_sent: AtomicU64::new(0),
                })
                .collect(),
        });
        let handle = Handle { shared };

        let resolvers = resolve::start(&handle.shared, &config.peers, config.timers)?;
        let driver = driver::Driver::new(handle.clone(), config, handler);
        let driver = std::thread::Builder::new()
            .name("niox-transport".into())
            .spawn(move || driver.run())?;

        Ok(Self {
            handle,
            driver: Some(driver),
            _resolvers: resolvers,
        })
    }

    /// A cloneable handle for sending from other threads.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }
}

impl Deref for Endpoint {
    type Target = Handle;

    fn deref(&self) -> &Handle {
        &self.handle
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let shared = &self.handle.shared;
        shared.stopped.store(true, Ordering::SeqCst);
        // Wake the driver out of recv with an empty datagram (which it
        // would drop anyway). Its read timeout is capped, so a lost wakeup
        // only delays the exit.
        let _ = shared.send_to(&[], shared.wake_addr());
        if let Some(driver) = self.driver.take()
            && driver.join().is_err()
        {
            warn!("transport driver thread panicked");
        }
        for peer in &shared.peers {
            peer.up.store(false, Ordering::Relaxed);
            *lock(&peer.route) = Route::default();
        }
    }
}

/// Sends to peers and reports their state. Cheap to clone, and usable
/// from any thread.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle")
            .field("local_addr", &self.shared.local_addr)
            .field("peers", &self.shared.peers.len())
            .finish_non_exhaustive()
    }
}

impl Handle {
    /// Encrypts `body` for `subsystem` and sends it to `peer`, on the
    /// calling thread. At most [`crate::MAX_BODY`] bytes. Nothing is
    /// queued or retried: if the peer is down the datagram is refused.
    pub fn send(&self, peer: PeerId, subsystem: Subsystem, body: &[u8]) -> Result<(), SendError> {
        self.shared
            .seal_and_send(peer.index(), Payload::Data(subsystem, body))
    }

    /// Whether `peer` currently has an established session.
    pub fn is_up(&self, peer: PeerId) -> bool {
        self.shared
            .peers
            .get(peer.index())
            .is_some_and(|p| p.up.load(Ordering::Relaxed))
    }

    pub fn peer_id(&self, name: &str) -> Option<PeerId> {
        self.shared
            .peers
            .iter()
            .position(|p| p.name == name)
            .map(PeerId::new)
    }

    /// The peer's configured name; empty for an unknown id.
    pub fn peer_name(&self, peer: PeerId) -> &str {
        self.shared
            .peers
            .get(peer.index())
            .map_or("", |p| p.name.as_str())
    }

    /// Every configured peer, in config order.
    pub fn peers(&self) -> impl Iterator<Item = PeerId> + '_ {
        (0..self.shared.peers.len()).map(PeerId::new)
    }

    /// The address the socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.shared.local_addr
    }
}

struct Shared {
    socket: UdpSocket,
    local_addr: SocketAddr,
    /// The socket is IPv6 (dual-stack), so IPv4 destinations are mapped.
    v6: bool,
    stopped: AtomicBool,
    /// Zero point for [`SharedPeer::last_sent`].
    epoch: Instant,
    peers: Box<[SharedPeer]>,
}

/// The per-peer state senders need. Everything else is the driver's alone.
struct SharedPeer {
    name: String,
    up: AtomicBool,
    route: Mutex<Route>,
    /// The configured address, once resolved.
    configured: Mutex<Option<SocketAddr>>,
    /// When anything was last sent to this peer, in nanoseconds since
    /// [`Shared::epoch`]; drives keepalives.
    last_sent: AtomicU64,
}

/// Where and how to send to a peer that's up.
#[derive(Default)]
struct Route {
    session: Option<Arc<Session>>,
    addr: Option<SocketAddr>,
}

impl Shared {
    fn seal_and_send(&self, peer: usize, payload: Payload<'_>) -> Result<(), SendError> {
        let entry = self.peers.get(peer).ok_or(SendError::NotConnected)?;
        let (session, addr) = {
            let route = lock(&entry.route);
            match (&route.session, route.addr) {
                (Some(session), Some(addr)) => (Arc::clone(session), addr),
                _ => return Err(SendError::NotConnected),
            }
        };
        let mut buf = [0; MAX_DATAGRAM];
        let len = session.seal(payload, &mut buf).map_err(SendError::Seal)?;
        self.send_to(&buf[..len], addr).map_err(SendError::Io)?;
        entry
            .last_sent
            .store(self.nanos_since_epoch(), Ordering::Relaxed);
        Ok(())
    }

    fn send_to(&self, datagram: &[u8], addr: SocketAddr) -> io::Result<()> {
        let addr = match addr {
            SocketAddr::V4(v4) if self.v6 => {
                SocketAddr::from((v4.ip().to_ipv6_mapped(), v4.port()))
            }
            other => other,
        };
        self.socket.send_to(datagram, addr).map(drop)
    }

    fn nanos_since_epoch(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// An address that reaches this socket, for the shutdown wakeup.
    fn wake_addr(&self) -> SocketAddr {
        let ip = match self.local_addr.ip() {
            ip if !ip.is_unspecified() => ip,
            _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        SocketAddr::new(ip, self.local_addr.port())
    }
}

/// Folds IPv4-mapped IPv6 addresses (as a dual-stack socket reports IPv4
/// sources) back to IPv4, so one peer's address always compares equal.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every value guarded here is consistent after each statement.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
