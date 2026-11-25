//! The driver thread: receives every datagram, runs each peer's handshake
//! and liveness state machine, and calls the handler.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use niox_config::{PrivateKey, PublicKey};
use tracing::{debug, info, trace, warn};

use super::{
    EndpointConfig, Event, Handle, PeerError, PeerId, PeerSpec, SharedPeer, Timers, canonical, lock,
};
use crate::packet::{INIT_LEN, RESPONSE_LEN};
use crate::{
    Datagram, Error, Initiation, MAX_DATAGRAM, MAX_PAYLOAD, Payload, Responded, Session, Timestamp,
    classify, initiate, respond,
};

/// Upper bound on one blocking receive, so a lost shutdown wakeup only
/// delays the exit.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// How many of a peer's inits the tie-break winner ignores before
/// answering one anyway.
const MAX_INITS_IGNORED: u32 = 3;

/// A change to the peers, carried out on the driver thread.
pub(super) enum Command {
    Add {
        id: PeerId,
        spec: PeerSpec,
        peer: Arc<SharedPeer>,
        reply: SyncSender<Result<(), PeerError>>,
    },
    Remove {
        id: PeerId,
        reply: SyncSender<Result<(), PeerError>>,
    },
    Update {
        id: PeerId,
        name: Arc<str>,
        reply: SyncSender<Result<(), PeerError>>,
    },
}

pub(super) struct Driver<F> {
    handle: Handle,
    key: PrivateKey,
    public_key: PublicKey,
    timers: Timers,
    /// Indexed by [`PeerId`]; `None` once removed.
    peers: Vec<Option<PeerState>>,
    commands: Receiver<Command>,
    handler: F,
}

struct PeerState {
    name: Arc<str>,
    public_key: PublicKey,
    /// What senders see of this peer.
    shared: Arc<SharedPeer>,
    /// The established session, if the peer is up.
    current: Option<Live>,
    /// A session this side created by answering an init, waiting for the
    /// peer's first datagram on it to confirm the handshake completed.
    unconfirmed: Option<(Arc<Session>, Instant)>,
    /// This side's outstanding handshake init.
    pending: Option<Pending>,
    /// When to next (re)send an init while down.
    retry_at: Instant,
    retry_delay: Duration,
    /// The newest init timestamp accepted from this peer.
    newest_init: Option<Timestamp>,
    /// The last init answered and the response sent, for answering its
    /// retransmissions.
    answered: Option<Box<([u8; INIT_LEN], [u8; RESPONSE_LEN])>>,
    /// The source of the last authenticated datagram from this peer.
    seen_at: Option<SocketAddr>,
    /// Inits from this peer ignored in favor of our pending one.
    inits_ignored: u32,
}

impl PeerState {
    fn new(spec: &PeerSpec, shared: Arc<SharedPeer>, timers: Timers, now: Instant) -> Self {
        Self {
            name: spec.name.as_str().into(),
            public_key: spec.public_key,
            shared,
            current: None,
            unconfirmed: None,
            pending: None,
            retry_at: now,
            retry_delay: timers.retry_initial,
            newest_init: None,
            answered: None,
            seen_at: None,
            inits_ignored: 0,
        }
    }
}

struct Live {
    session: Arc<Session>,
    last_received: Instant,
}

struct Pending {
    initiation: Initiation,
    created: Instant,
}

impl<F> Driver<F>
where
    F: FnMut(&Handle, Event<'_>),
{
    pub(super) fn new(
        handle: Handle,
        config: EndpointConfig,
        shared: Vec<Arc<SharedPeer>>,
        commands: Receiver<Command>,
        handler: F,
    ) -> Self {
        let now = Instant::now();
        let peers = config
            .peers
            .into_iter()
            .zip(shared)
            .map(|(spec, shared)| Some(PeerState::new(&spec, shared, config.timers, now)))
            .collect();
        Self {
            handle,
            public_key: config.private_key.public_key(),
            key: config.private_key,
            timers: config.timers,
            peers,
            commands,
            handler,
        }
    }

    /// Every current peer, with its index.
    fn live(&self) -> impl Iterator<Item = (usize, &PeerState)> {
        self.peers
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i, s)))
    }

    fn state(&self, index: usize) -> Option<&PeerState> {
        self.peers.get(index)?.as_ref()
    }

    fn state_mut(&mut self, index: usize) -> Option<&mut PeerState> {
        self.peers.get_mut(index)?.as_mut()
    }

    fn on_commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Add {
                    id,
                    spec,
                    peer,
                    reply,
                } => {
                    let result = if self.live().any(|(_, s)| s.public_key == spec.public_key)
                        || spec.public_key == self.public_key
                    {
                        Err(PeerError::DuplicateKey)
                    } else if self.peers.get(id.index()).is_some_and(Option::is_some) {
                        Err(PeerError::TooMany)
                    } else {
                        self.add(id, &spec, peer);
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                Command::Remove { id, reply } => {
                    let result = if self.state(id.index()).is_some() {
                        self.remove(id.index());
                        Ok(())
                    } else {
                        Err(PeerError::NoSuchPeer)
                    };
                    let _ = reply.send(result);
                }
                Command::Update { id, name, reply } => {
                    let now = Instant::now();
                    let retry_initial = self.timers.retry_initial;
                    let result = match self.state_mut(id.index()) {
                        Some(state) => {
                            if *state.name != *name {
                                info!(peer = %state.name, new_name = %name, "peer renamed");
                            }
                            state.name = name;
                            // A new address may be reachable where the old
                            // one wasn't: try it now.
                            if state.current.is_none() {
                                state.retry_at = now;
                                state.retry_delay = retry_initial;
                            }
                            Ok(())
                        }
                        None => Err(PeerError::NoSuchPeer),
                    };
                    let _ = reply.send(result);
                }
            }
        }
    }

    fn add(&mut self, id: PeerId, spec: &PeerSpec, peer: Arc<SharedPeer>) {
        let index = id.index();
        {
            let mut shared = self
                .handle
                .shared
                .peers
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if shared.len() <= index {
                shared.resize(index + 1, None);
            }
            shared[index] = Some(Arc::clone(&peer));
        }
        if self.peers.len() <= index {
            self.peers.resize_with(index + 1, || None);
        }
        self.peers[index] = Some(PeerState::new(spec, peer, self.timers, Instant::now()));
        info!(peer = spec.name, address = %spec.address, "peer added");
    }

    fn remove(&mut self, index: usize) {
        self.go_down(index, Instant::now());
        let Some(state) = self.peers.get_mut(index).and_then(Option::take) else {
            return;
        };
        if let Some(slot) = self
            .handle
            .shared
            .peers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(index)
        {
            *slot = None;
        }
        state.shared.up.store(false, Ordering::Relaxed);
        *lock(&state.shared.route) = super::Route::default();
        info!(peer = %state.name, "peer removed");
    }

    pub(super) fn run(mut self) {
        // One spare byte, so an oversized datagram is seen as such rather
        // than silently truncated to a valid length.
        let mut buf = [0; MAX_DATAGRAM + 1];
        let mut plaintext = [0; MAX_PAYLOAD];
        let mut timeout = None;

        loop {
            if self.handle.shared.stopped.load(Ordering::SeqCst) {
                return;
            }
            self.on_commands();
            let now = Instant::now();
            self.on_timers(now);

            let wait = self
                .next_deadline(now)
                .saturating_duration_since(Instant::now())
                .clamp(Duration::from_millis(1), MAX_WAIT);
            // Round up to whole milliseconds so the timeout is only reset
            // when it changes meaningfully, not before every datagram.
            let wait = Duration::from_millis(u64::try_from(wait.as_millis()).unwrap_or(1).max(1));
            let socket = &self.handle.shared.socket;
            if timeout != Some(wait) {
                if let Err(e) = socket.set_read_timeout(Some(wait)) {
                    warn!("setting transport socket timeout: {e}");
                }
                timeout = Some(wait);
            }

            match socket.recv_from(&mut buf) {
                Ok((len, src)) => self.on_datagram(&buf[..len], canonical(src), &mut plaintext),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    // e.g. a queued ICMP error; nothing to do but go on.
                    debug!("transport socket receive: {e}");
                }
            }
        }
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let epoch = self.handle.shared.epoch;
        let mut deadline = now + MAX_WAIT;
        for (_, state) in self.live() {
            if let Some(live) = &state.current {
                let last_sent = state.shared.last_sent(epoch);
                deadline = deadline
                    .min(live.last_received + self.timers.dead_after)
                    .min(last_sent + self.timers.keepalive);
            } else {
                deadline = deadline.min(state.retry_at);
            }
            if let Some((_, created)) = &state.unconfirmed {
                deadline = deadline.min(*created + self.timers.dead_after);
            }
        }
        deadline
    }

    fn on_timers(&mut self, now: Instant) {
        for index in 0..self.peers.len() {
            let Some(state) = self.peers[index].as_mut() else {
                continue;
            };
            if state
                .unconfirmed
                .as_ref()
                .is_some_and(|(_, created)| now >= *created + self.timers.dead_after)
            {
                debug!(peer = %state.name, "unconfirmed session expired");
                state.unconfirmed = None;
            }

            if let Some(live) = &state.current {
                if now >= live.last_received + self.timers.dead_after {
                    info!(peer = %state.name, "peer stopped responding");
                    self.go_down(index, now);
                    self.send_init(index, now);
                    continue;
                }
                let last_sent = state.shared.last_sent(self.handle.shared.epoch);
                if now >= last_sent + self.timers.keepalive {
                    self.send_keepalive(index);
                }
            } else if now >= state.retry_at {
                self.send_init(index, now);
            }
        }
    }

    fn on_datagram(&mut self, datagram: &[u8], src: SocketAddr, plaintext: &mut [u8]) {
        let now = Instant::now();
        match classify(datagram) {
            Ok(Datagram::Init) => self.on_init(datagram, src, now),
            Ok(Datagram::Response { receiver_index }) => {
                self.on_response(datagram, receiver_index, src, now);
            }
            Ok(Datagram::Data { receiver_index, .. }) => {
                self.on_data(datagram, receiver_index, src, now, plaintext);
            }
            Err(e) => trace!(%src, "dropped datagram: {e}"),
        }
    }

    fn on_init(&mut self, init: &[u8], src: SocketAddr, now: Instant) {
        // A retransmission of an init already answered: resend the answer.
        // (respond() would reject it as a replay.)
        if let Some((_, state)) = self
            .live()
            .find(|(_, s)| s.answered.as_ref().is_some_and(|a| a.0 == init))
        {
            if let Some(answered) = &state.answered {
                trace!(peer = %state.name, %src, "resending handshake response");
                let response = answered.1;
                let _ = self.handle.shared.send_to(&response, src);
            }
            return;
        }

        // Try the peer this address belongs to first, then everyone else.
        let mut order: Vec<usize> = self.live().map(|(i, _)| i).collect();
        order.sort_by_key(|&i| !self.is_peer_address(i, src));

        for index in order {
            let local_index = self.new_index();
            let Some(state) = self.state(index) else {
                continue;
            };
            match respond(
                &self.key,
                &state.public_key,
                init,
                local_index,
                state.newest_init,
            ) {
                Ok(responded) => {
                    self.on_init_accepted(index, init, responded, src, now);
                    return;
                }
                Err(Error::Auth) => {}
                Err(e) => {
                    debug!(peer = %state.name, %src, "dropped handshake init: {e}");
                    return;
                }
            }
        }
        trace!(%src, "dropped handshake init from no configured peer");
    }

    fn on_init_accepted(
        &mut self,
        index: usize,
        init: &[u8],
        responded: Responded,
        src: SocketAddr,
        now: Instant,
    ) {
        let public_key = self.public_key;
        let dead_after = self.timers.dead_after;
        let shared = Arc::clone(&self.handle.shared);
        let Some(state) = self.state_mut(index) else {
            return;
        };
        let we_initiate = public_key.as_bytes() < state.public_key.as_bytes();

        // Both sides initiating at once: the lower public key's init wins.
        // Nudge the peer with ours right away instead of waiting out the
        // retry delay. Only hold out for a few rounds, though: if the peer
        // keeps initiating, our init isn't getting through (say it rejects
        // the timestamp because our clock stepped back), and answering
        // theirs is the only way up.
        if we_initiate
            && let Some(pending) = &state.pending
            && state.inits_ignored < MAX_INITS_IGNORED
        {
            state.inits_ignored += 1;
            debug!(peer = %state.name, "simultaneous handshake; keeping ours");
            let _ = shared.send_to(pending.initiation.datagram(), src);
            return;
        }

        debug!(peer = %state.name, %src, "answering handshake");
        state.newest_init = Some(responded.timestamp);
        let mut init_bytes = [0; INIT_LEN];
        init_bytes.copy_from_slice(init);
        state.answered = Some(Box::new((init_bytes, responded.datagram)));
        state.unconfirmed = Some((Arc::new(responded.session), now));
        // The peer drives this handshake; only start our own again if it
        // never confirms.
        state.pending = None;
        state.inits_ignored = 0;
        if state.current.is_none() {
            state.retry_at = state.retry_at.max(now + dead_after);
        }
        if let Err(e) = shared.send_to(&responded.datagram, src) {
            debug!(peer = %state.name, %src, "sending handshake response: {e}");
        }
    }

    fn on_response(&mut self, response: &[u8], receiver_index: u32, src: SocketAddr, now: Instant) {
        let Some(index) = self.live().find_map(|(i, s)| {
            s.pending
                .as_ref()
                .is_some_and(|p| p.initiation.local_index() == receiver_index)
                .then_some(i)
        }) else {
            trace!(%src, "dropped handshake response for no pending handshake");
            return;
        };
        let Some(state) = self.state_mut(index) else {
            return;
        };
        let Some(pending) = &mut state.pending else {
            return;
        };
        match pending.initiation.complete(response) {
            Ok(session) => {
                state.seen_at = Some(src);
                self.establish(index, Arc::new(session), now);
                // Confirm the session to the responder right away.
                self.send_keepalive(index);
            }
            Err(e) => debug!(peer = %state.name, %src, "dropped handshake response: {e}"),
        }
    }

    fn on_data(
        &mut self,
        datagram: &[u8],
        receiver_index: u32,
        src: SocketAddr,
        now: Instant,
        plaintext: &mut [u8],
    ) {
        let found = self.live().find_map(|(index, s)| {
            if let Some(live) = &s.current
                && live.session.local_index() == receiver_index
            {
                return Some((index, Arc::clone(&live.session), false));
            }
            match &s.unconfirmed {
                Some((session, _)) if session.local_index() == receiver_index => {
                    Some((index, Arc::clone(session), true))
                }
                _ => None,
            }
        });
        let Some((index, session, confirms)) = found else {
            trace!(%src, "dropped data for unknown session index {receiver_index:#010x}");
            return;
        };

        let payload = match session.open(datagram, plaintext) {
            Ok(payload) => payload,
            Err(e) => {
                if let Some(state) = self.state(index) {
                    trace!(peer = %state.name, %src, "dropped data: {e}");
                }
                return;
            }
        };

        if confirms && let Some(state) = self.state_mut(index) {
            state.unconfirmed = None;
            state.seen_at = Some(src);
            self.establish(index, session, now);
        }
        let Some(state) = self.state_mut(index) else {
            return;
        };
        if let Some(live) = &mut state.current {
            live.last_received = now;
        }
        if state.seen_at != Some(src) {
            info!(peer = %state.name, %src, "peer moved to a new address");
            state.seen_at = Some(src);
            lock(&state.shared.route).addr = Some(src);
        }

        if let Payload::Data(subsystem, body) = payload {
            (self.handler)(
                &self.handle,
                Event::Data {
                    peer: PeerId::new(index),
                    subsystem,
                    body,
                },
            );
        }
    }

    /// Makes `session` the peer's current one and reports it up (after
    /// reporting the old session down, if there was one).
    fn establish(&mut self, index: usize, session: Arc<Session>, now: Instant) {
        let Some(state) = self.state(index) else {
            return;
        };
        if state.current.is_some() {
            info!(peer = %state.name, "peer re-handshook");
            self.go_down(index, now);
        }
        let retry_initial = self.timers.retry_initial;
        let Some(state) = self.state_mut(index) else {
            return;
        };
        state.pending = None;
        state.inits_ignored = 0;
        state.retry_delay = retry_initial;
        let shared = &state.shared;
        {
            let mut route = lock(&shared.route);
            route.session = Some(Arc::clone(&session));
            route.addr = state.seen_at.or(*lock(&shared.configured));
        }
        state.current = Some(Live {
            session,
            last_received: now,
        });
        shared.up.store(true, Ordering::Relaxed);
        info!(peer = %state.name, addr = ?state.seen_at, "peer up");
        (self.handler)(&self.handle, Event::Up(PeerId::new(index)));
    }

    fn go_down(&mut self, index: usize, now: Instant) {
        let retry_initial = self.timers.retry_initial;
        let Some(state) = self.state_mut(index) else {
            return;
        };
        if state.current.take().is_none() {
            return;
        }
        state.pending = None;
        state.retry_delay = retry_initial;
        state.retry_at = now;
        state.shared.up.store(false, Ordering::Relaxed);
        lock(&state.shared.route).session = None;
        info!(peer = %state.name, "peer down");
        (self.handler)(&self.handle, Event::Down(PeerId::new(index)));
    }

    fn send_keepalive(&self, index: usize) {
        let Some(state) = self.state(index) else {
            return;
        };
        let shared = &self.handle.shared;
        if let Err(e) = shared.seal_and_send_to(&state.shared, Payload::Keepalive) {
            debug!(peer = %state.name, "sending keepalive: {e:#}");
            // Count the attempt anyway, or the keepalive deadline stays in
            // the past and this retries every millisecond while the
            // network is down.
            state
                .shared
                .last_sent
                .store(shared.nanos_since_epoch(), Ordering::Relaxed);
        }
    }

    /// Sends (or retransmits) this side's init, and schedules the next
    /// retry.
    fn send_init(&mut self, index: usize, now: Instant) {
        let Some(state) = self.state(index) else {
            return;
        };
        let stale = state
            .pending
            .as_ref()
            .is_none_or(|p| now >= p.created + self.timers.init_lifetime);
        if stale {
            let local_index = self.new_index();
            let key = self.key.clone();
            let Some(state) = self.state_mut(index) else {
                return;
            };
            match initiate(&key, &state.public_key, local_index, Timestamp::now()) {
                Ok(initiation) => {
                    state.pending = Some(Pending {
                        initiation,
                        created: now,
                    });
                }
                Err(e) => warn!(peer = %state.name, "starting handshake: {e}"),
            }
        }

        let (retry_max, shared) = (self.timers.retry_max, Arc::clone(&self.handle.shared));
        let Some(state) = self.state_mut(index) else {
            return;
        };
        let configured = *lock(&state.shared.configured);
        if let Some(pending) = &state.pending {
            let mut targets = [configured, state.seen_at];
            if targets[0] == targets[1] {
                targets[1] = None;
            }
            for addr in targets.into_iter().flatten() {
                trace!(peer = %state.name, %addr, "sending handshake init");
                if let Err(e) = shared.send_to(pending.initiation.datagram(), addr) {
                    debug!(peer = %state.name, %addr, "sending handshake init: {e}");
                }
            }
        }

        state.retry_at = now + jitter(state.retry_delay);
        state.retry_delay = (state.retry_delay * 2).min(retry_max);
    }

    fn is_peer_address(&self, index: usize, addr: SocketAddr) -> bool {
        self.state(index)
            .is_some_and(|s| s.seen_at == Some(addr) || *lock(&s.shared.configured) == Some(addr))
    }

    /// A random session index not used by any current, unconfirmed, or
    /// pending session.
    fn new_index(&self) -> u32 {
        loop {
            let index = random_u32();
            let in_use = self.live().any(|(_, s)| {
                s.current
                    .as_ref()
                    .is_some_and(|l| l.session.local_index() == index)
                    || s.unconfirmed
                        .as_ref()
                        .is_some_and(|(u, _)| u.local_index() == index)
                    || s.pending
                        .as_ref()
                        .is_some_and(|p| p.initiation.local_index() == index)
            });
            if !in_use {
                return index;
            }
        }
    }
}

/// Up to 25% extra, so two peers that went down together don't retry in
/// lockstep.
fn jitter(delay: Duration) -> Duration {
    delay + delay.mul_f64(f64::from(random_u32() % 1024) / 4096.0)
}

fn random_u32() -> u32 {
    // The OS random source doesn't fail on any supported platform. If it
    // somehow did, the counter still yields distinct (if predictable)
    // values, so index allocation always terminates.
    static FALLBACK: AtomicU32 = AtomicU32::new(0);
    let mut bytes = [0; 4];
    let _ = getrandom::fill(&mut bytes);
    u32::from_ne_bytes(bytes) ^ FALLBACK.fetch_add(0x9E37_79B9, Ordering::Relaxed)
}
