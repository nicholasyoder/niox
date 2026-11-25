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
//!
//! # Changing peers
//!
//! Peers can be added, removed, and updated while the endpoint runs
//! ([`Endpoint::add_peer`], [`Endpoint::remove_peer`],
//! [`Endpoint::update_peer`]), for a config reload. Every other peer's
//! session carries on untouched. A [`PeerId`] is never reused, so an id
//! held for a removed peer can't reach a different one.

mod driver;
mod resolve;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};
use std::thread::{JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use niox_config::{Config, PeerAddress, PrivateKey, PublicKey};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::warn;

use crate::{Error, MAX_DATAGRAM, Payload, Session, Subsystem};

/// Identifies one peer of an [`Endpoint`]. The peers in
/// [`EndpointConfig::peers`] are numbered from 0 in order; each peer added
/// later gets the next number. Ids are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(u32);

impl PeerId {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The peer at `index` in [`EndpointConfig::peers`] (for
    /// [`EndpointConfig::from_config`], its index in the config's peers).
    pub fn new(index: usize) -> Self {
        // Peer counts are tiny; the endpoint caps them.
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
    driver_id: ThreadId,
    commands: mpsc::Sender<driver::Command>,
    timers: Timers,
    /// Per peer configured by hostname; dropping one stops its resolver.
    resolvers: HashMap<PeerId, resolve::Stop>,
    next_id: usize,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

/// Why changing an [`Endpoint`]'s peers failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PeerError {
    #[error("no such peer")]
    NoSuchPeer,
    #[error("too many peers")]
    TooMany,
    #[error("a peer with that public key is already configured")]
    DuplicateKey,
    #[error("starting the peer's address resolver")]
    Resolver(#[source] io::Error),
    /// The driver thread is gone (it panicked).
    #[error("the transport driver thread has stopped")]
    Stopped,
}

/// Most peers an endpoint takes over its lifetime, counting removed ones.
const MAX_PEER_IDS: usize = u16::MAX as usize;

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
        if config.peers.len() > MAX_PEER_IDS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many peers",
            ));
        }
        let local_addr = socket.local_addr()?;
        let peers: Vec<Arc<SharedPeer>> = config
            .peers
            .iter()
            .map(|spec| Arc::new(SharedPeer::new(&spec.name)))
            .collect();
        let shared = Arc::new(Shared {
            v6: local_addr.is_ipv6(),
            local_addr,
            socket,
            stopped: AtomicBool::new(false),
            epoch: Instant::now(),
            peers: RwLock::new(peers.iter().cloned().map(Some).collect()),
        });
        let handle = Handle { shared };

        let mut resolvers = HashMap::new();
        for (index, (spec, peer)) in config.peers.iter().zip(&peers).enumerate() {
            if let Some(stop) = resolve::start(
                handle.shared.v6,
                peer,
                &spec.name,
                &spec.address,
                config.timers,
            )? {
                resolvers.insert(PeerId::new(index), stop);
            }
        }
        let (commands, command_rx) = mpsc::channel();
        let timers = config.timers;
        let next_id = config.peers.len();
        let driver = driver::Driver::new(handle.clone(), config, peers, command_rx, handler);
        let driver = std::thread::Builder::new()
            .name("niox-transport".into())
            .spawn(move || driver.run())?;

        Ok(Self {
            handle,
            driver_id: driver.thread().id(),
            driver: Some(driver),
            commands,
            timers,
            resolvers,
            next_id,
        })
    }

    /// The id the next [`add_peer`](Self::add_peer) will return, so a
    /// caller can be ready for the new peer's events before it's added.
    pub fn next_peer_id(&self) -> PeerId {
        PeerId::new(self.next_id)
    }

    /// Adds a peer, which starts handshaking at once. Fails if its public
    /// key is already in use.
    ///
    /// This and the other peer-changing methods wait for the driver
    /// thread, so they must not be called from the handler, nor while
    /// holding anything the handler waits for.
    pub fn add_peer(&mut self, spec: PeerSpec) -> Result<PeerId, PeerError> {
        if self.next_id >= MAX_PEER_IDS {
            return Err(PeerError::TooMany);
        }
        let id = PeerId::new(self.next_id);
        let peer = Arc::new(SharedPeer::new(&spec.name));
        let resolver = resolve::start(
            self.handle.shared.v6,
            &peer,
            &spec.name,
            &spec.address,
            self.timers,
        )
        .map_err(PeerError::Resolver)?;
        self.command(|reply| driver::Command::Add {
            id,
            spec: spec.clone(),
            peer,
            reply,
        })??;
        self.next_id += 1;
        if let Some(stop) = resolver {
            self.resolvers.insert(id, stop);
        }
        Ok(id)
    }

    /// Removes a peer. If it was up, the handler first gets its
    /// [`Event::Down`].
    pub fn remove_peer(&mut self, id: PeerId) -> Result<(), PeerError> {
        self.command(|reply| driver::Command::Remove { id, reply })??;
        self.resolvers.remove(&id);
        Ok(())
    }

    /// Renames a peer and changes its configured address, keeping its
    /// session. (A different public key is a different peer: remove the
    /// old one and add the new.) While the peer is down, the next
    /// handshake goes out at once.
    pub fn update_peer(
        &mut self,
        id: PeerId,
        name: &str,
        address: PeerAddress,
    ) -> Result<(), PeerError> {
        let peer = self.handle.shared.peer(id).ok_or(PeerError::NoSuchPeer)?;
        let name: Arc<str> = name.into();
        *lock(&peer.name) = Arc::clone(&name);
        // Replacing the resolver stops the old one.
        *lock(&peer.configured) = None;
        self.resolvers.remove(&id);
        if let Some(stop) =
            resolve::start(self.handle.shared.v6, &peer, &name, &address, self.timers)
                .map_err(PeerError::Resolver)?
        {
            self.resolvers.insert(id, stop);
        }
        self.command(|reply| driver::Command::Update { id, name, reply })?
    }

    /// Sends a command to the driver thread and waits for its reply.
    fn command<T>(
        &self,
        make: impl FnOnce(mpsc::SyncSender<T>) -> driver::Command,
    ) -> Result<T, PeerError> {
        debug_assert!(
            std::thread::current().id() != self.driver_id,
            "changing peers from the transport handler would deadlock"
        );
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .send(make(reply))
            .map_err(|_| PeerError::Stopped)?;
        let shared = &self.handle.shared;
        // Wake the driver out of recv; see Drop.
        let _ = shared.send_to(&[], shared.wake_addr());
        result.recv().map_err(|_| PeerError::Stopped)
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
        for peer in read(&shared.peers).iter().flatten() {
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
            .field("peers", &read(&self.shared.peers).iter().flatten().count())
            .finish_non_exhaustive()
    }
}

impl Handle {
    /// Encrypts `body` for `subsystem` and sends it to `peer`, on the
    /// calling thread. At most [`crate::MAX_BODY`] bytes. Nothing is
    /// queued or retried: if the peer is down the datagram is refused.
    pub fn send(&self, peer: PeerId, subsystem: Subsystem, body: &[u8]) -> Result<(), SendError> {
        self.shared
            .seal_and_send(peer, Payload::Data(subsystem, body))
    }

    /// Whether `peer` currently has an established session.
    pub fn is_up(&self, peer: PeerId) -> bool {
        self.shared
            .peer(peer)
            .is_some_and(|p| p.up.load(Ordering::Relaxed))
    }

    pub fn peer_id(&self, name: &str) -> Option<PeerId> {
        read(&self.shared.peers)
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| **lock(&p.name) == *name))
            .map(PeerId::new)
    }

    /// The peer's configured name; empty for an unknown id.
    pub fn peer_name(&self, peer: PeerId) -> Arc<str> {
        self.shared
            .peer(peer)
            .map_or_else(|| Arc::from(""), |p| Arc::clone(&lock(&p.name)))
    }

    /// Every current peer, in the order they were added.
    pub fn peers(&self) -> Vec<PeerId> {
        read(&self.shared.peers)
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_some())
            .map(|(i, _)| PeerId::new(i))
            .collect()
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
    /// Indexed by [`PeerId`]; `None` once removed. Only the driver thread
    /// changes it.
    peers: RwLock<Vec<Option<Arc<SharedPeer>>>>,
}

/// The per-peer state senders need. Everything else is the driver's alone.
struct SharedPeer {
    name: Mutex<Arc<str>>,
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

impl SharedPeer {
    fn new(name: &str) -> Self {
        Self {
            name: Mutex::new(name.into()),
            up: AtomicBool::new(false),
            route: Mutex::new(Route::default()),
            configured: Mutex::new(None),
            last_sent: AtomicU64::new(0),
        }
    }

    fn last_sent(&self, epoch: Instant) -> Instant {
        epoch + Duration::from_nanos(self.last_sent.load(Ordering::Relaxed))
    }
}

impl Shared {
    fn peer(&self, id: PeerId) -> Option<Arc<SharedPeer>> {
        read(&self.peers).get(id.index())?.clone()
    }

    fn seal_and_send(&self, peer: PeerId, payload: Payload<'_>) -> Result<(), SendError> {
        let entry = self.peer(peer).ok_or(SendError::NotConnected)?;
        self.seal_and_send_to(&entry, payload)
    }

    fn seal_and_send_to(&self, entry: &SharedPeer, payload: Payload<'_>) -> Result<(), SendError> {
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

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}
