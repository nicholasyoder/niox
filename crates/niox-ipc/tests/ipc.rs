//! The control socket end to end, over a socket in a temporary directory.

// Test helpers outside #[test] fns are not covered by clippy.toml.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use niox_ipc::audio::{MAX_PACKET, Packet};
use niox_ipc::protocol::PROTOCOL_VERSION;
use niox_ipc::{
    AudioChannel, Client, ClientError, Event, Handler, Message, PeerStatus, Publisher, Request,
    Role, Server, ServerError, Status, Target,
};

/// Records focus requests; accepts the host and connected clients.
#[derive(Clone)]
struct FakeKm {
    publisher: Publisher,
    requests: Arc<Mutex<Vec<Target>>>,
    /// nioxd's end of the audio channel, as last handed over; and every
    /// handover, as whether it gave a channel.
    audio: Arc<Mutex<(Option<AudioChannel>, Vec<bool>)>>,
}

impl Handler for FakeKm {
    fn focus(&self, target: &Target) -> Result<(), String> {
        self.requests.lock().unwrap().push(target.clone());
        if let Target::Client(name) = target {
            let status = self.publisher.status();
            match status.peer(name) {
                Some(p) if p.role == Role::Client && p.in_focus_cycle => {}
                Some(_) => return Err(format!("{name:?} isn't connected")),
                None => return Err(format!("no client named {name:?}")),
            }
        }
        self.publisher.update(|s| s.focus = Some(target.clone()));
        Ok(())
    }

    /// Pretends the config dropped "laptop", unless it already did.
    fn reload(&self) -> Result<Vec<String>, String> {
        if self.publisher.status().peer("laptop").is_none() {
            return Err("invalid config: nothing left to remove".into());
        }
        self.publisher
            .update(|s| s.peers.retain(|p| p.name != "laptop"));
        Ok(vec!["peer \"laptop\" removed".into()])
    }

    fn audio_channel(&self, channel: Option<AudioChannel>) {
        let mut audio = self.audio.lock().unwrap();
        audio.1.push(channel.is_some());
        audio.0 = channel;
    }

    /// Nobody: these tests' connections all have the same uid anyway.
    fn active_uid(&self) -> Option<u32> {
        None
    }
}

impl FakeKm {
    fn new(publisher: Publisher) -> Self {
        Self {
            publisher,
            requests: Arc::default(),
            audio: Arc::default(),
        }
    }

    /// Waits until the audio channel has been handed over `n` times in
    /// all, and returns those handovers.
    fn handovers(&self, n: usize) -> Vec<bool> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got = self.audio.lock().unwrap().1.clone();
            if got.len() >= n || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn status() -> Status {
    Status {
        version: "0.1.0".into(),
        protocol: PROTOCOL_VERSION,
        public_key: "cHVibGljIGtleQ==".into(),
        port: 4777,
        focus: Some(Target::Host),
        peers: vec![
            peer("desk", Role::Client, true),
            peer("laptop", Role::Client, false),
            peer("office", Role::Host, true),
        ],
    }
}

fn peer(name: &str, role: Role, connected: bool) -> PeerStatus {
    PeerStatus {
        name: name.into(),
        address: format!("{name}.lan"),
        role,
        connected,
        in_focus_cycle: connected && role == Role::Client,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    km: FakeKm,
    server: Option<Server>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_group(None)
    }

    fn with_group(group: Option<u32>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/niox.sock");
        let publisher = Publisher::new(status());
        let km = FakeKm::new(publisher.clone());
        let server = Server::start(&path, group, publisher, km.clone()).unwrap();
        Self {
            _dir: dir,
            path,
            km,
            server: Some(server),
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.path).unwrap()
    }

    fn raw(&self) -> (UnixStream, BufReader<UnixStream>) {
        let s = UnixStream::connect(&self.path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let r = BufReader::new(s.try_clone().unwrap());
        (s, r)
    }
}

fn raw_request(s: &mut UnixStream, r: &mut BufReader<UnixStream>, line: &str) -> Message {
    s.write_all(line.as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
    let mut reply = String::new();
    r.read_line(&mut reply).unwrap();
    serde_json::from_str(&reply).unwrap()
}

#[test]
fn status_reports_the_published_status() {
    let f = Fixture::new();
    assert_eq!(f.client().status().unwrap(), status());

    f.km.publisher.update(|s| s.peers[1].connected = true);
    assert!(f.client().status().unwrap().peers[1].connected);
}

#[test]
fn focus_goes_to_the_handler() {
    let f = Fixture::new();
    let mut c = f.client();
    c.focus(Target::Client("desk".into())).unwrap();
    assert_eq!(
        c.status().unwrap().focus,
        Some(Target::Client("desk".into()))
    );
    c.focus(Target::Host).unwrap();

    match c.focus(Target::Client("laptop".into())) {
        Err(ClientError::Refused(m)) => assert_eq!(m, r#""laptop" isn't connected"#),
        other => panic!("{other:?}"),
    }
    match c.focus(Target::Client("nope".into())) {
        Err(ClientError::Refused(m)) => assert_eq!(m, r#"no client named "nope""#),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        *f.km.requests.lock().unwrap(),
        vec![
            Target::Client("desk".into()),
            Target::Host,
            Target::Client("laptop".into()),
            Target::Client("nope".into()),
        ]
    );
    // Refusals leave the connection usable.
    assert_eq!(c.status().unwrap().focus, Some(Target::Host));
}

#[test]
fn subscribers_get_a_snapshot_then_each_change() {
    let f = Fixture::new();
    let (snapshot, mut events) = f.client().subscribe().unwrap();
    assert_eq!(snapshot, status());

    // Nothing changes: nothing sent.
    f.km.publisher.update(|_| {});
    f.km.publisher.update(|s| {
        s.peers[1].connected = true;
        s.peers[1].in_focus_cycle = true;
    });
    f.client().focus(Target::Client("laptop".into())).unwrap();
    f.km.publisher.update(|s| s.peers.truncate(1));

    let mut laptop = peer("laptop", Role::Client, true);
    laptop.in_focus_cycle = true;
    assert_eq!(events.next_event().unwrap(), Some(Event::Peer(laptop)));
    assert_eq!(
        events.next_event().unwrap(),
        Some(Event::Focus {
            focus: Target::Client("laptop".into())
        })
    );
    match events.next_event().unwrap() {
        Some(Event::Status(s)) => assert_eq!(s.peers.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_subscribed_connection_still_answers_requests() {
    let f = Fixture::new();
    let (mut s, mut r) = f.raw();
    assert!(matches!(
        raw_request(&mut s, &mut r, r#"{"cmd":"subscribe"}"#),
        Message::Status(_)
    ));
    f.km.publisher.update(|s| s.peers[0].connected = false);
    // The event queued first arrives first, then the reply.
    assert!(matches!(
        raw_request(&mut s, &mut r, r#"{"cmd":"focus","target":"host"}"#),
        Message::Event(Event::Peer(_))
    ));
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert_eq!(line, "{\"type\":\"ok\"}\n");
}

#[test]
fn bad_requests_get_an_error_and_the_connection_survives() {
    let f = Fixture::new();
    let (mut s, mut r) = f.raw();
    for bad in [
        "not json",
        r#"{"cmd":"reboot"}"#,
        r#"{"cmd":"focus"}"#,
        "[]",
    ] {
        match raw_request(&mut s, &mut r, bad) {
            Message::Error { message } => assert!(message.starts_with("invalid request"), "{bad}"),
            other => panic!("{bad}: {other:?}"),
        }
    }
    // Blank lines are ignored.
    s.write_all(b"\n  \n").unwrap();
    assert!(matches!(
        raw_request(&mut s, &mut r, r#"{"cmd":"status"}"#),
        Message::Status(_)
    ));
}

#[test]
fn an_overlong_line_closes_the_connection() {
    let f = Fixture::new();
    let (mut s, mut r) = f.raw();
    let long = vec![b'x'; niox_ipc::MAX_LINE + 10];
    // The server may close before reading it all.
    let _ = s.write_all(&long);
    // An error line, then the end; or, since the server closes with our
    // input unread, a reset that can discard that line.
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => assert!(line.contains("longer than"), "{line}"),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => panic!("{e}"),
        }
    }
    // The server is still fine.
    assert!(f.client().status().is_ok());
}

#[test]
fn a_subscriber_that_stops_reading_never_blocks_updates() {
    let f = Fixture::new();
    let (mut s, mut r) = f.raw();
    raw_request(&mut s, &mut r, r#"{"cmd":"subscribe"}"#);
    let start = Instant::now();
    for i in 0..20_000 {
        f.km.publisher.update(|s| s.peers[0].connected = i % 2 == 0);
    }
    assert!(start.elapsed() < Duration::from_secs(5));
    // It was disconnected: reading drains what was sent, then ends.
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => panic!("{e}"),
        }
    }
    // Others are unaffected.
    assert!(f.client().status().is_ok());
}

#[test]
fn socket_permissions() {
    let f = Fixture::new();
    let meta = std::fs::metadata(&f.path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    let gid = nix::unistd::getgid().as_raw();
    let f = Fixture::with_group(Some(gid));
    let meta = std::fs::metadata(&f.path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o660);
    assert_eq!(meta.gid(), gid);
    // No temporary socket left behind.
    let dir = f.path.parent().unwrap();
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);
}

#[test]
fn a_stale_socket_is_replaced_but_a_live_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niox.sock");
    // Left behind by a process that's gone.
    drop(UnixListener::bind(&path).unwrap());
    let a = start(&path).unwrap();
    assert!(Client::connect(&path).unwrap().status().is_ok());

    assert!(matches!(start(&path), Err(ServerError::InUse(_))));
    // The refused start didn't disturb the running one.
    assert!(Client::connect(&path).unwrap().status().is_ok());
    drop(a);

    std::fs::write(&path, "not a socket").unwrap();
    assert!(matches!(start(&path), Err(ServerError::NotSocket(_))));
}

fn start(path: &Path) -> Result<Server, ServerError> {
    let publisher = Publisher::new(status());
    let km = FakeKm::new(publisher.clone());
    Server::start(path, None, publisher, km)
}

#[test]
fn stopping_closes_connections_and_removes_the_socket() {
    let mut f = Fixture::new();
    let (_, mut events) = f.client().subscribe().unwrap();
    let mut idle = f.client();
    drop(f.server.take());

    assert!(!f.path.exists());
    assert_eq!(events.next_event().unwrap(), None);
    assert!(matches!(
        idle.request(&Request::Status),
        Err(ClientError::Closed | ClientError::Io(_))
    ));
    assert!(matches!(
        Client::connect(&f.path),
        Err(ClientError::Connect { .. })
    ));
}

#[test]
fn reload_reports_changes_and_subscribers_see_the_new_status() {
    let f = Fixture::new();
    let (_, mut events) = f.client().subscribe().unwrap();
    let mut c = f.client();
    assert_eq!(c.reload().unwrap(), ["peer \"laptop\" removed"]);
    let Some(Ok(Event::Status(status))) = events.next() else {
        panic!("expected a full status event");
    };
    assert!(status.peer("laptop").is_none());
    // A failed reload is a refusal with the reason, and the connection
    // carries on.
    match c.reload() {
        Err(ClientError::Refused(m)) => assert!(m.contains("invalid config"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(c.status().unwrap().peers.len(), 2);
}

#[test]
fn the_audio_channel_goes_to_the_first_to_ask_and_moves_on_when_it_leaves() {
    let f = Fixture::new();
    // Asking first and subscribing after: the grant can arrive before the
    // subscription's snapshot, and is kept for it.
    let mut a = f.client();
    a.request_audio().unwrap();
    let (_, mut a_events) = a.subscribe().unwrap();
    assert_eq!(
        a_events.next_event().unwrap(),
        Some(Event::Audio { granted: true })
    );
    let a_channel = a_events.take_audio_channel().unwrap();
    assert!(a_events.take_audio_channel().is_none());
    assert_eq!(f.km.handovers(1), [true]);

    // Packets cross both ways.
    let mut buf = [0; MAX_PACKET];
    {
        let audio = f.km.audio.lock().unwrap();
        let nioxd = audio.0.as_ref().unwrap();
        nioxd.send("desk", &[1, 2]).unwrap();
        assert_eq!(
            a_channel.recv(&mut buf).unwrap(),
            Some(Packet {
                peer: "desk",
                body: &[1, 2]
            })
        );
        a_channel.send("office", &[3]).unwrap();
        assert_eq!(
            nioxd.recv(&mut buf).unwrap(),
            Some(Packet {
                peer: "office",
                body: &[3]
            })
        );
    }

    // A second asker waits.
    let mut b = f.client();
    b.request_audio().unwrap();
    let (_, mut b_events) = b.subscribe().unwrap();
    // Asking twice changes nothing.
    let mut again = f.client();
    again.request_audio().unwrap();
    drop(again);
    assert_eq!(f.km.handovers(1), [true]);

    // The holder going away (as on `kill -9`) frees the channel for the
    // next in line, with a new socketpair.
    drop(a_events);
    assert_eq!(
        b_events.next_event().unwrap(),
        Some(Event::Audio { granted: true })
    );
    let b_channel = b_events.take_audio_channel().unwrap();
    assert_eq!(f.km.handovers(2), [true, true]);
    // The old channel ended along with nioxd's end of it.
    assert_eq!(a_channel.recv(&mut buf).unwrap(), None);
    b_channel.send("desk", &[4]).unwrap();
    assert_eq!(
        f.km.audio
            .lock()
            .unwrap()
            .0
            .as_ref()
            .unwrap()
            .recv(&mut buf)
            .unwrap(),
        Some(Packet {
            peer: "desk",
            body: &[4]
        })
    );

    // Nobody left: nioxd is told to close its end.
    drop(b_events);
    assert_eq!(f.km.handovers(3), [true, true, false]);
    assert!(f.km.audio.lock().unwrap().0.is_none());
    assert_eq!(b_channel.recv(&mut buf).unwrap(), None);
}

#[test]
fn an_audio_request_is_answered_ok() {
    let f = Fixture::new();
    let (mut s, mut r) = f.raw();
    assert_eq!(
        raw_request(&mut s, &mut r, r#"{"cmd":"audio"}"#),
        Message::Ok
    );
    // The grant follows, as an event line.
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert_eq!(
        line,
        "{\"type\":\"event\",\"event\":\"audio\",\"granted\":true}\n"
    );
}

#[test]
fn stopping_ends_the_audio_channel() {
    let mut f = Fixture::new();
    let mut a = f.client();
    a.request_audio().unwrap();
    let (_, mut events) = a.subscribe().unwrap();
    events.next_event().unwrap();
    let channel = events.take_audio_channel().unwrap();
    drop(f.server.take());
    assert_eq!(f.km.handovers(2), [true, false]);
    let mut buf = [0; MAX_PACKET];
    assert_eq!(channel.recv(&mut buf).unwrap(), None);
}
