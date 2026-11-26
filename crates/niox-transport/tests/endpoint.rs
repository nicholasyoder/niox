//! Endpoint tests over real loopback sockets, with short timers: multiple
//! peers, restarts, partitions, lost handshake datagrams, strangers, and
//! address learning.

// Test helpers outside #[test] fns are not covered by clippy.toml.
#![allow(clippy::unwrap_used)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use niox_config::{PeerAddress, PrivateKey, PublicKey};
use niox_transport::{
    Endpoint, EndpointConfig, Event, PeerError, PeerId, PeerSpec, SendError, Subsystem, Timers,
    Timestamp, initiate,
};

const TIMERS: Timers = Timers {
    keepalive: Duration::from_millis(50),
    dead_after: Duration::from_millis(300),
    retry_initial: Duration::from_millis(20),
    retry_max: Duration::from_millis(100),
    init_lifetime: Duration::from_secs(1),
    resolve_interval: Duration::from_secs(1),
};

/// Generous, so a loaded CI machine doesn't flake; tests normally finish
/// far sooner.
const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Up(PeerId),
    Down(PeerId),
    Data(PeerId, Subsystem, Vec<u8>),
}

/// A peer's identity and its socket. The socket stays bound for the
/// node's whole life, across endpoint restarts, so no parallel test can
/// take its port in between.
struct Node {
    key: PrivateKey,
    socket: UdpSocket,
    port: u16,
}

impl Node {
    fn new() -> Self {
        Self::on(UdpSocket::bind("127.0.0.1:0").unwrap())
    }

    /// A node using `socket`, which may be bound to any loopback-reachable
    /// address.
    fn on(socket: UdpSocket) -> Self {
        let port = socket.local_addr().unwrap().port();
        Self {
            key: PrivateKey::generate().unwrap(),
            socket,
            port,
        }
    }

    fn public(&self) -> PublicKey {
        self.key.public_key()
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    fn spec(&self, name: &str) -> PeerSpec {
        spec(name, self.public(), self.addr())
    }

    /// Another handle on this node's socket, with anything queued while
    /// no endpoint was running discarded, as a freshly bound socket would
    /// have nothing queued.
    fn socket(&self) -> UdpSocket {
        self.socket.set_nonblocking(true).unwrap();
        let mut buf = [0; 2048];
        while self.socket.recv(&mut buf).is_ok() {}
        self.socket.set_nonblocking(false).unwrap();
        self.socket.try_clone().unwrap()
    }

    /// Starts an endpoint on this node's socket. Only one may run at a
    /// time.
    fn start(&self, peers: Vec<PeerSpec>) -> (Endpoint, Receiver<Ev>) {
        start(self.socket(), &self.key, peers)
    }
}

fn spec(name: &str, public_key: PublicKey, addr: SocketAddr) -> PeerSpec {
    PeerSpec {
        name: name.into(),
        public_key,
        address: PeerAddress::parse(&addr.to_string(), 0).unwrap(),
    }
}

fn start(socket: UdpSocket, key: &PrivateKey, peers: Vec<PeerSpec>) -> (Endpoint, Receiver<Ev>) {
    let (tx, rx) = mpsc::channel();
    let config = EndpointConfig {
        private_key: key.clone(),
        peers,
        timers: TIMERS,
    };
    let endpoint = Endpoint::start(socket, config, move |_, event| {
        let ev = match event {
            Event::Up(p) => Ev::Up(p),
            Event::Down(p) => Ev::Down(p),
            Event::Data {
                peer,
                subsystem,
                body,
            } => Ev::Data(peer, subsystem, body.to_vec()),
        };
        let _ = tx.send(ev);
    })
    .unwrap();
    (endpoint, rx)
}

/// Waits for `want`, skipping data and keepalive-driven noise but failing
/// on anything unexpected among up/down events.
fn expect(rx: &Receiver<Ev>, want: &Ev) {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(ev) if ev == *want => return,
            Ok(Ev::Data(..)) => {}
            Ok(other) => panic!("expected {want:?}, got {other:?}"),
            Err(e) => panic!("expected {want:?}: {e}"),
        }
    }
}

/// Asserts nothing but data arrives for `dur`.
fn expect_quiet(rx: &Receiver<Ev>, dur: Duration) {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Ev::Data(..)) => {}
            Ok(other) => panic!("expected no events, got {other:?}"),
            Err(RecvTimeoutError::Timeout) => return,
            Err(e) => panic!("{e}"),
        }
    }
}

/// Sends `body` from `from` to its peer and waits for it to arrive.
fn round_trip(from: &Endpoint, peer: PeerId, to_rx: &Receiver<Ev>, from_id: PeerId, body: &[u8]) {
    let deadline = Instant::now() + WAIT;
    // A send can race the other side's confirmation; retry briefly.
    loop {
        match from.send(peer, Subsystem::Km, body) {
            Ok(()) => break,
            Err(SendError::NotConnected) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("send: {e}"),
        }
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match to_rx.recv_timeout(left) {
            Ok(Ev::Data(p, Subsystem::Km, b)) if p == from_id && b == body => return,
            Ok(Ev::Data(..) | Ev::Up(_)) => {}
            Ok(other) => panic!("waiting for data, got {other:?}"),
            Err(e) => panic!("waiting for data: {e}"),
        }
    }
}

#[test]
fn host_with_two_clients_exchanges_data() {
    // The host binds dual-stack on every interface; clients reach it over
    // IPv4 as mapped addresses.
    let host = Node::on(Endpoint::bind(0).unwrap());
    let (a, b) = (Node::new(), Node::new());
    let (eh, rh) = host.start(vec![a.spec("a"), b.spec("b")]);
    let (ea, ra) = a.start(vec![host.spec("host")]);
    let (eb, rb) = b.start(vec![host.spec("host")]);

    let (ha, hb) = (eh.peer_id("a").unwrap(), eh.peer_id("b").unwrap());
    let h_on_a = ea.peer_id("host").unwrap();
    let h_on_b = eb.peer_id("host").unwrap();
    assert_eq!(&*eh.peer_name(hb), "b");

    expect(&ra, &Ev::Up(h_on_a));
    expect(&rb, &Ev::Up(h_on_b));

    round_trip(&eh, ha, &ra, h_on_a, b"to a");
    round_trip(&eh, hb, &rb, h_on_b, b"to b");
    round_trip(&ea, h_on_a, &rh, ha, b"from a");
    round_trip(&eb, h_on_b, &rh, hb, b"from b");
    eb.send(h_on_b, Subsystem::Audio, &[9; 1000]).unwrap();

    // Several keepalive periods: sessions stay up, with no spurious
    // down/up churn on any side.
    expect_quiet(&ra, TIMERS.dead_after * 3);
    expect_quiet(&rb, Duration::ZERO);
    let mut ups = 0;
    while let Ok(ev) = rh.try_recv() {
        match ev {
            Ev::Up(_) => ups += 1,
            Ev::Data(..) => {}
            Ev::Down(p) => panic!("host saw {p:?} go down"),
        }
    }
    assert!(ups <= 2, "host saw {ups} ups");
    assert!(eh.is_up(ha) && eh.is_up(hb));
}

#[test]
fn sending_to_a_down_peer_is_refused() {
    let (a, b) = (Node::new(), Node::new());
    let (ea, _ra) = a.start(vec![b.spec("b")]);
    let peer = ea.peer_id("b").unwrap();
    assert!(!ea.is_up(peer));
    assert!(matches!(
        ea.send(peer, Subsystem::Km, b"x"),
        Err(SendError::NotConnected)
    ));
}

#[test]
fn either_side_restarting_recovers() {
    let (a, b) = (Node::new(), Node::new());
    let (mut ea, mut ra) = a.start(vec![b.spec("b")]);
    let (mut eb, mut rb) = b.start(vec![a.spec("a")]);
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));

    // Restart A quickly: B replaces its session (down, then up) as soon as
    // A's new handshake is confirmed, without waiting to time out.
    drop(ea);
    let started = Instant::now();
    (ea, ra) = a.start(vec![b.spec("b")]);
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Down(pa));
    expect(&rb, &Ev::Up(pa));
    assert!(
        started.elapsed() < TIMERS.dead_after,
        "{:?}",
        started.elapsed()
    );
    round_trip(&ea, pb, &rb, pa, b"a restarted");
    round_trip(&eb, pa, &ra, pb, b"b still here");

    // Restart B, slowly enough that A notices it's gone first.
    drop(eb);
    expect(&ra, &Ev::Down(pb));
    (eb, rb) = b.start(vec![a.spec("a")]);
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));
    round_trip(&eb, pa, &ra, pb, b"b restarted");

    // Both at once.
    drop((ea, eb));
    (ea, ra) = a.start(vec![b.spec("b")]);
    (eb, rb) = b.start(vec![a.spec("a")]);
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));
    round_trip(&ea, pb, &rb, pa, b"both restarted");
    round_trip(&eb, pa, &ra, pb, b"both restarted");
}

/// Forwards datagrams between two endpoints through two sockets: A talks
/// to `a_side`, B to `b_side`. Can block all traffic or drop chosen
/// datagrams, and logs what it forwarded.
struct Relay {
    a_side: SocketAddr,
    b_side: SocketAddr,
    blocked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    log: Log,
    threads: Vec<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    AToB,
    BToA,
}

type Log = Arc<Mutex<Vec<(Dir, Vec<u8>)>>>;
type Running = (Endpoint, Receiver<Ev>);

type Filter = Arc<Mutex<Box<dyn FnMut(Dir, &[u8]) -> bool + Send>>>;

impl Relay {
    fn new(
        a: SocketAddr,
        b: SocketAddr,
        filter: impl FnMut(Dir, &[u8]) -> bool + Send + 'static,
    ) -> Self {
        let a_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut relay = Self {
            a_side: a_sock.local_addr().unwrap(),
            b_side: b_sock.local_addr().unwrap(),
            blocked: Arc::default(),
            stop: Arc::default(),
            log: Arc::default(),
            threads: Vec::new(),
        };
        let filter: Filter = Arc::new(Mutex::new(Box::new(filter)));
        let (a_sock, b_sock) = (Arc::new(a_sock), Arc::new(b_sock));
        for (dir, from, to, dest) in [
            (Dir::AToB, Arc::clone(&a_sock), Arc::clone(&b_sock), b),
            (Dir::BToA, b_sock, a_sock, a),
        ] {
            let (blocked, stop, log, filter) = (
                Arc::clone(&relay.blocked),
                Arc::clone(&relay.stop),
                Arc::clone(&relay.log),
                Arc::clone(&filter),
            );
            from.set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            relay.threads.push(std::thread::spawn(move || {
                let mut buf = [0; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let Ok(len) = from.recv(&mut buf) else {
                        continue;
                    };
                    let d = &buf[..len];
                    if blocked.load(Ordering::Relaxed) || !(filter.lock().unwrap())(dir, d) {
                        continue;
                    }
                    log.lock().unwrap().push((dir, d.to_vec()));
                    let _ = to.send_to(d, dest);
                }
            }));
        }
        relay
    }

    fn forwarded(&self, dir: Dir, kind: u8) -> Vec<Vec<u8>> {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|(d, bytes)| *d == dir && bytes[1] == kind)
            .map(|(_, bytes)| bytes.clone())
            .collect()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            t.join().unwrap();
        }
    }
}

/// A and B connected through a relay: each is configured with the relay's
/// address for the other. A has the lower public key, so it wins the
/// simultaneous-handshake tie-break.
fn relayed(filter: impl FnMut(Dir, &[u8]) -> bool + Send + 'static) -> (Relay, Running, Running) {
    let (mut a, mut b) = (Node::new(), Node::new());
    if a.public().as_bytes() > b.public().as_bytes() {
        std::mem::swap(&mut a, &mut b);
    }
    let relay = Relay::new(a.addr(), b.addr(), filter);
    let ea = a.start(vec![spec("b", b.public(), relay.a_side)]);
    let eb = b.start(vec![spec("a", a.public(), relay.b_side)]);
    (relay, ea, eb)
}

#[test]
fn network_partition_recovers() {
    let (relay, (ea, ra), (eb, rb)) = relayed(|_, _| true);
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));

    relay.blocked.store(true, Ordering::Relaxed);
    expect(&ra, &Ev::Down(pb));
    expect(&rb, &Ev::Down(pa));
    assert!(matches!(
        ea.send(pb, Subsystem::Km, b"x"),
        Err(SendError::NotConnected)
    ));
    // Stay partitioned long enough for the retry delay to hit its cap.
    std::thread::sleep(TIMERS.retry_max * 4);

    relay.blocked.store(false, Ordering::Relaxed);
    let healed = Instant::now();
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));
    // One capped retry period, plus slack for confirmation.
    assert!(
        healed.elapsed() < TIMERS.retry_max * 3,
        "{:?}",
        healed.elapsed()
    );
    round_trip(&ea, pb, &rb, pa, b"healed");
    round_trip(&eb, pa, &ra, pb, b"healed");
}

#[test]
fn lost_response_is_answered_from_cache() {
    // Drop B's first handshake response and, so A is the only initiator,
    // every init B sends.
    let mut dropped = false;
    let (relay, (ea, ra), (eb, rb)) = relayed(move |dir, d| match (dir, d[1]) {
        (Dir::BToA, 1) => false,
        (Dir::BToA, 2) if !dropped => {
            dropped = true;
            false
        }
        _ => true,
    });
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));

    // A retransmitted the very same init, and B answered the
    // retransmission with the response it cached.
    let inits = relay.forwarded(Dir::AToB, 1);
    assert!(inits.len() >= 2, "{} inits", inits.len());
    assert!(inits.iter().all(|i| *i == inits[0]));
    assert_eq!(relay.forwarded(Dir::BToA, 2).len(), 1);
    round_trip(&ea, pb, &rb, pa, b"after retransmit");
}

#[test]
fn lost_confirmation_is_recovered_by_later_traffic() {
    // Drop A's first data datagram (the confirming keepalive): B holds its
    // session unconfirmed until A's next keepalive.
    let mut dropped = false;
    let (_relay, (ea, ra), (eb, rb)) = relayed(move |dir, d| match (dir, d[1]) {
        (Dir::BToA, 1) => false,
        (Dir::AToB, 3) if !dropped => {
            dropped = true;
            false
        }
        _ => true,
    });
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));
    round_trip(&eb, pa, &ra, pb, b"confirmed");
}

#[test]
fn strangers_get_no_reply() {
    let (a, b, stranger) = (Node::new(), Node::new(), Node::new());
    let (ea, ra) = a.start(vec![b.spec("b")]);
    let (eb, rb) = b.start(vec![a.spec("a")]);
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));

    // A peer A doesn't know, which knows A's key; garbage; and data for
    // session indices that don't exist.
    let probe = stranger.socket();
    probe
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let init = initiate(&stranger.key, &a.public(), 1, Timestamp::now()).unwrap();
    let mut data = vec![1, 3, 0, 0];
    data.extend_from_slice(&[0xEE; 40]);
    for d in [init.datagram(), b"hello", &[1, 1, 0, 0], &data] {
        probe.send_to(d, a.addr()).unwrap();
    }
    let mut buf = [0; 2048];
    assert!(probe.recv(&mut buf).is_err(), "A replied to a stranger");

    expect_quiet(&ra, Duration::from_millis(100));
    round_trip(&ea, pb, &rb, pa, b"unaffected");
}

#[test]
fn learns_a_peers_address_from_authenticated_traffic() {
    let (a, b) = (Node::new(), Node::new());
    // A has a wrong address for B (nothing listens there), so only B's
    // inits can bring the pair up, and A must reply to where they came
    // from.
    let nowhere = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (ea, ra) = a.start(vec![spec("b", b.public(), nowhere.local_addr().unwrap())]);
    let (eb, rb) = b.start(vec![a.spec("a")]);
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));
    round_trip(&ea, pb, &rb, pa, b"found you");
    expect_quiet(&ra, TIMERS.dead_after * 2);
}

#[test]
fn resolves_hostnames() {
    let (a, b) = (Node::new(), Node::new());
    let by_name = |name: &str, node: &Node| PeerSpec {
        name: name.into(),
        public_key: node.public(),
        address: PeerAddress::parse(&format!("localhost:{}", node.port), 0).unwrap(),
    };
    let (ea, ra) = a.start(vec![by_name("b", &b)]);
    let (eb, rb) = b.start(vec![by_name("a", &a)]);
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));
}

#[test]
fn dropping_the_endpoint_stops_promptly() {
    let (a, b) = (Node::new(), Node::new());
    let (ea, _ra) = a.start(vec![b.spec("b")]);
    let handle = ea.handle();
    let peer = handle.peer_id("b").unwrap();
    let started = Instant::now();
    drop(ea);
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(matches!(
        handle.send(peer, Subsystem::Km, b"x"),
        Err(SendError::NotConnected)
    ));
}

#[test]
fn tie_break_winner_yields_when_its_init_cannot_get_through() {
    // A wins the tie-break, but its inits never arrive (as when B rejects
    // their timestamps). A ignores a few of B's inits, then answers one.
    let (_relay, (ea, ra), (eb, rb)) = relayed(|dir, d| !(dir == Dir::AToB && d[1] == 1));
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));
    round_trip(&ea, pb, &rb, pa, b"b initiated");
}

#[test]
fn peers_added_at_runtime_connect_without_disturbing_others() {
    let (host, a, b) = (Node::new(), Node::new(), Node::new());
    let (mut eh, rh) = host.start(vec![a.spec("a")]);
    let (ea, ra) = a.start(vec![host.spec("host")]);
    let (eb, rb) = b.start(vec![host.spec("host")]);
    let ha = eh.peer_id("a").unwrap();
    expect(&ra, &Ev::Up(ea.peer_id("host").unwrap()));
    expect(&rh, &Ev::Up(ha));

    let hb = eh.add_peer(b.spec("b")).unwrap();
    assert_ne!(hb, ha);
    assert_eq!(eh.peer_id("b"), Some(hb));
    let h_on_b = eb.peer_id("host").unwrap();
    expect(&rb, &Ev::Up(h_on_b));
    expect(&rh, &Ev::Up(hb));
    round_trip(&eh, hb, &rb, h_on_b, b"welcome");

    // A's session was never touched.
    expect_quiet(&ra, TIMERS.dead_after * 2);
    assert!(eh.is_up(ha));

    // A key that's already configured (or this endpoint's own) is refused.
    assert!(matches!(
        eh.add_peer(a.spec("again")),
        Err(PeerError::DuplicateKey)
    ));
    assert!(matches!(
        eh.add_peer(host.spec("me")),
        Err(PeerError::DuplicateKey)
    ));
}

#[test]
fn removing_a_peer_reports_it_down_and_forgets_it() {
    let (host, a, b) = (Node::new(), Node::new(), Node::new());
    let (mut eh, rh) = host.start(vec![a.spec("a"), b.spec("b")]);
    let (_ea, ra) = a.start(vec![host.spec("host")]);
    let (_eb, _rb) = b.start(vec![host.spec("host")]);
    let (ha, hb) = (eh.peer_id("a").unwrap(), eh.peer_id("b").unwrap());
    expect(&ra, &Ev::Up(PeerId::new(0)));
    let mut up = Vec::new();
    while up.len() < 2 {
        match rh.recv_timeout(WAIT).unwrap() {
            Ev::Up(p) => up.push(p),
            Ev::Data(..) => {}
            Ev::Down(p) => panic!("{p:?} went down"),
        }
    }

    eh.remove_peer(hb).unwrap();
    expect(&rh, &Ev::Down(hb));
    assert_eq!(eh.peer_id("b"), None);
    assert_eq!(eh.peers(), vec![ha]);
    assert!(eh.peer_name(hb).is_empty());
    assert!(matches!(
        eh.send(hb, Subsystem::Km, b"x"),
        Err(SendError::NotConnected)
    ));
    assert!(matches!(eh.remove_peer(hb), Err(PeerError::NoSuchPeer)));
    // B keeps trying to handshake, and is ignored; A stays up.
    expect_quiet(&rh, TIMERS.dead_after * 2);
    assert!(eh.is_up(ha));

    // Re-adding it gets a fresh id.
    let again = eh.add_peer(b.spec("b")).unwrap();
    assert_ne!(again, hb);
    expect(&rh, &Ev::Up(again));
}

#[test]
fn updating_a_peer_keeps_its_session_and_follows_the_new_address() {
    let (a, b) = (Node::new(), Node::new());
    let nowhere = UdpSocket::bind("127.0.0.1:0").unwrap();
    // Each side has a wrong address for the other, so nothing connects
    // until A's config is fixed.
    let nowhere = nowhere.local_addr().unwrap();
    let (mut ea, ra) = a.start(vec![spec("b", b.public(), nowhere)]);
    let (eb, rb) = b.start(vec![spec("a", a.public(), nowhere)]);
    let (pb, pa) = (ea.peer_id("b").unwrap(), eb.peer_id("a").unwrap());
    expect_quiet(&ra, TIMERS.retry_max);

    ea.update_peer(
        pb,
        "bee",
        PeerAddress::parse(&b.addr().to_string(), 0).unwrap(),
    )
    .unwrap();
    assert_eq!(&*ea.peer_name(pb), "bee");
    assert_eq!(ea.peer_id("bee"), Some(pb));
    expect(&ra, &Ev::Up(pb));
    expect(&rb, &Ev::Up(pa));

    // Renaming again while up doesn't disturb the session.
    ea.update_peer(
        pb,
        "b",
        PeerAddress::parse(&b.addr().to_string(), 0).unwrap(),
    )
    .unwrap();
    round_trip(&ea, pb, &rb, pa, b"same session");
    expect_quiet(&ra, TIMERS.dead_after * 2);
    assert!(matches!(
        ea.update_peer(
            PeerId::new(7),
            "x",
            PeerAddress::parse("127.0.0.1", 1).unwrap()
        ),
        Err(PeerError::NoSuchPeer)
    ));
}
