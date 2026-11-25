//! The host side of the KM protocol, free of devices, sockets, and clocks.

use std::time::{Duration, Instant};

use niox_config::Hotkey;

use crate::caps::Capabilities;
use crate::event::InputEvent;
use crate::focus::{FocusRing, Target, Unavailable};
use crate::held::HeldKeys;
use crate::hotkey::Switcher;
use crate::wire::{MAX_FRAME_EVENTS, MAX_MESSAGE, Message, WireError, capabilities_len};

/// First resend delay for unacknowledged capabilities; doubles per resend.
pub const CAPS_RESEND_INITIAL: Duration = Duration::from_millis(250);
/// Longest delay between capabilities resends.
pub const CAPS_RESEND_MAX: Duration = Duration::from_secs(2);

/// Something for the caller to do, produced by [`HostKm`] in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostOutput<P> {
    /// Play a frame on the host's own passthrough device (then
    /// `SYN_REPORT`).
    Local(Vec<InputEvent>),
    /// Send a message to a client.
    Send { peer: P, message: Message },
    /// Focus moved to a new target.
    Focus(Target<P>),
}

/// Capabilities too large for one message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("device capabilities don't fit in one message")]
pub struct CapabilitiesTooLarge(#[source] WireError);

#[derive(Debug)]
struct Client<P> {
    peer: P,
    /// A session is up.
    up: bool,
    /// Keys held on this client.
    held: HeldKeys,
    /// The next frame's sequence number; restarts with each session.
    next_seq: u32,
    /// Capabilities sent but not yet acknowledged.
    pending: Option<Pending>,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    generation: u32,
    due: Instant,
    backoff: Duration,
}

/// The host's KM state: the hotkey, focus, which keys are held where, and
/// the capabilities exchange with each client.
///
/// The caller feeds it captured frames and transport events, and carries
/// out the [`HostOutput`]s it returns. A client joins the focus cycle once
/// it has a session *and* has acknowledged capabilities at least once in
/// that session (so it has a device to play into). Whenever focus leaves a
/// target, everything held there is released. When a client's session
/// drops, the client releases its own keys.
#[derive(Debug)]
pub struct HostKm<P> {
    switcher: Switcher,
    ring: FocusRing<P>,
    clients: Vec<Client<P>>,
    host_held: HeldKeys,
    caps: Capabilities,
    generation: u32,
}

impl<P: Copy + Eq> HostKm<P> {
    /// `clients` in focus-cycling order (normally config order), and the
    /// union of the captured devices' capabilities.
    pub fn new(
        hotkey: Hotkey,
        clients: impl IntoIterator<Item = P>,
        caps: Capabilities,
    ) -> Result<Self, CapabilitiesTooLarge> {
        check_caps(&caps)?;
        let ring = FocusRing::new(clients);
        let clients = ring
            .clients()
            .map(|peer| Client {
                peer,
                up: false,
                held: HeldKeys::new(),
                next_seq: 0,
                pending: None,
            })
            .collect();
        Ok(Self {
            switcher: Switcher::new(hotkey),
            ring,
            clients,
            host_held: HeldKeys::new(),
            caps,
            generation: 1,
        })
    }

    pub fn focus(&self) -> Target<P> {
        self.ring.focused()
    }

    /// Clients in the focus cycle (connected, with a device), in order.
    pub fn ready_clients(&self) -> impl Iterator<Item = P> + '_ {
        self.ring.available()
    }

    /// Processes one captured frame: the events up to a `SYN_REPORT`
    /// (boundaries, if included, are ignored).
    pub fn process_frame(&mut self, frame: &[InputEvent], out: &mut Vec<HostOutput<P>>) {
        let mut current = Vec::with_capacity(frame.len() + 1);
        for ev in frame.iter().filter(|ev| !ev.is_frame_boundary()) {
            let decision = self.switcher.process(ev);
            if decision.press_key1_first {
                let press = self.switcher.key1_press();
                if self.held_mut(self.focus()).filter(&press) {
                    current.push(press);
                }
            }
            if decision.forward && self.held_mut(self.focus()).filter(ev) {
                current.push(*ev);
            }
            if decision.switch {
                let old = self.focus();
                self.emit(old, std::mem::take(&mut current), out);
                if let Some(new) = self.ring.advance() {
                    self.release(old, out);
                    out.push(HostOutput::Focus(new));
                }
            }
        }
        self.emit(self.focus(), current, out);
    }

    /// Moves focus to `target` (e.g. on a `nioxctl focus` request).
    pub fn set_focus(
        &mut self,
        target: Target<P>,
        out: &mut Vec<HostOutput<P>>,
    ) -> Result<(), Unavailable> {
        let old = self.focus();
        if let Some(new) = self.ring.focus(target)? {
            self.release(old, out);
            out.push(HostOutput::Focus(new));
        }
        Ok(())
    }

    /// A session with `peer` came up: start sending it capabilities.
    /// Peers that aren't configured clients are ignored.
    pub fn peer_up(&mut self, peer: P, now: Instant, out: &mut Vec<HostOutput<P>>) {
        let generation = self.generation;
        let caps = self.caps.clone();
        let Some(c) = self.client_mut(peer) else {
            return;
        };
        // Down always precedes a replacement Up, but don't rely on it.
        c.up = true;
        c.held.clear();
        c.next_seq = 0;
        c.pending = Some(Pending {
            generation,
            due: now + CAPS_RESEND_INITIAL,
            backoff: CAPS_RESEND_INITIAL,
        });
        out.push(HostOutput::Send {
            peer,
            message: Message::Capabilities { generation, caps },
        });
    }

    /// The session with `peer` is gone. If it had focus, focus returns to
    /// the host. Nothing is sent: the client releases its own keys.
    pub fn peer_down(&mut self, peer: P, out: &mut Vec<HostOutput<P>>) {
        let Some(c) = self.client_mut(peer) else {
            return;
        };
        c.up = false;
        c.held.clear();
        c.pending = None;
        if let Some(new) = self.ring.set_available(peer, false) {
            out.push(HostOutput::Focus(new));
        }
    }

    /// Handles a message from a client. Only acknowledgments are expected;
    /// anything else is returned as an error for the caller to log.
    pub fn on_message(&mut self, peer: P, message: &Message) -> Result<(), UnexpectedMessage> {
        let &Message::CapabilitiesAck { generation } = message else {
            return Err(UnexpectedMessage);
        };
        let Some(c) = self.client_mut(peer) else {
            return Err(UnexpectedMessage);
        };
        if !c.up {
            return Ok(());
        }
        if c.pending.is_some_and(|p| p.generation == generation) {
            c.pending = None;
        }
        // Any ack from this session means the client has a device.
        self.ring.set_available(peer, true);
        Ok(())
    }

    /// The captured devices changed. If the capabilities differ, the new
    /// set is sent to every connected client (which recreates its device
    /// only if they grew).
    pub fn set_capabilities(
        &mut self,
        caps: Capabilities,
        now: Instant,
        out: &mut Vec<HostOutput<P>>,
    ) -> Result<(), CapabilitiesTooLarge> {
        if caps == self.caps {
            return Ok(());
        }
        check_caps(&caps)?;
        self.caps = caps;
        self.generation = self.generation.wrapping_add(1);
        let peers: Vec<P> = self
            .clients
            .iter()
            .filter(|c| c.up)
            .map(|c| c.peer)
            .collect();
        for peer in peers {
            self.send_caps(peer, now, CAPS_RESEND_INITIAL, out);
        }
        Ok(())
    }

    /// Resends any capabilities whose acknowledgment is overdue.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<HostOutput<P>>) {
        let due: Vec<(P, Duration)> = self
            .clients
            .iter()
            .filter_map(|c| {
                let p = c.pending?;
                (p.due <= now).then_some((c.peer, p.backoff))
            })
            .collect();
        for (peer, backoff) in due {
            let next = (backoff * 2).min(CAPS_RESEND_MAX);
            self.send_caps(peer, now, next, out);
        }
    }

    /// When [`HostKm::poll`] next has work, if ever.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.clients
            .iter()
            .filter_map(|c| c.pending.map(|p| p.due))
            .min()
    }

    fn send_caps(
        &mut self,
        peer: P,
        now: Instant,
        backoff: Duration,
        out: &mut Vec<HostOutput<P>>,
    ) {
        let generation = self.generation;
        let caps = self.caps.clone();
        if let Some(c) = self.client_mut(peer) {
            c.pending = Some(Pending {
                generation,
                due: now + backoff,
                backoff,
            });
            out.push(HostOutput::Send {
                peer,
                message: Message::Capabilities { generation, caps },
            });
        }
    }

    fn release(&mut self, target: Target<P>, out: &mut Vec<HostOutput<P>>) {
        let events = self.held_mut(target).release_all();
        self.emit(target, events, out);
    }

    fn emit(&mut self, target: Target<P>, events: Vec<InputEvent>, out: &mut Vec<HostOutput<P>>) {
        if events.is_empty() {
            return;
        }
        match target {
            Target::Host => out.push(HostOutput::Local(events)),
            Target::Client(peer) => {
                let Some(c) = self.client_mut(peer) else {
                    return;
                };
                for chunk in events.chunks(MAX_FRAME_EVENTS) {
                    let seq = c.next_seq;
                    c.next_seq = seq.wrapping_add(1);
                    out.push(HostOutput::Send {
                        peer,
                        message: Message::Frame {
                            seq,
                            events: chunk.to_vec(),
                        },
                    });
                }
            }
        }
    }

    fn held_mut(&mut self, target: Target<P>) -> &mut HeldKeys {
        match target {
            Target::Client(peer) => {
                // Focus is only ever on a configured client.
                if let Some(i) = self.clients.iter().position(|c| c.peer == peer) {
                    return &mut self.clients[i].held;
                }
                &mut self.host_held
            }
            Target::Host => &mut self.host_held,
        }
    }

    fn client_mut(&mut self, peer: P) -> Option<&mut Client<P>> {
        self.clients.iter_mut().find(|c| c.peer == peer)
    }
}

fn check_caps(caps: &Capabilities) -> Result<(), CapabilitiesTooLarge> {
    let len = capabilities_len(caps);
    if len > MAX_MESSAGE {
        return Err(CapabilitiesTooLarge(WireError::TooLarge(len)));
    }
    Ok(())
}

/// A client sent something other than a capabilities acknowledgment, or a
/// peer that isn't a configured client sent anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unexpected KM message from a client")]
pub struct UnexpectedMessage;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EV_KEY, EV_MSC, EV_REL, EV_SYN, KEY_DOWN, KEY_REPEAT, KEY_UP, SYN_REPORT};

    use HostOutput::{Focus, Local, Send};
    use Target::{Client, Host};

    const META: u16 = 125;
    const GRAVE: u16 = 41;
    const A: u16 = 30;
    const SHIFT: u16 = 42;

    fn key(code: u16, value: i32) -> InputEvent {
        InputEvent::key(code, value)
    }

    fn caps() -> Capabilities {
        let mut c = Capabilities::new();
        c.insert(EV_KEY, A).unwrap();
        c
    }

    struct Rig {
        km: HostKm<char>,
        now: Instant,
    }

    impl Rig {
        fn new(clients: &[char]) -> Self {
            let hotkey = Hotkey {
                key1: META,
                key2: Some(GRAVE),
            };
            Self {
                km: HostKm::new(hotkey, clients.iter().copied(), caps()).unwrap(),
                now: Instant::now(),
            }
        }

        /// Brings `peer` up and acks its capabilities.
        fn connect(&mut self, peer: char) {
            let mut out = vec![];
            self.km.peer_up(peer, self.now, &mut out);
            let [Send { message, .. }] = &out[..] else {
                panic!("expected one caps send, got {out:?}");
            };
            let Message::Capabilities { generation, .. } = message else {
                panic!("expected caps, got {message:?}");
            };
            self.km
                .on_message(
                    peer,
                    &Message::CapabilitiesAck {
                        generation: *generation,
                    },
                )
                .unwrap();
        }

        fn frame(&mut self, events: &[InputEvent]) -> Vec<HostOutput<char>> {
            let mut out = vec![];
            self.km.process_frame(events, &mut out);
            out
        }

        fn press(&mut self, code: u16) -> Vec<HostOutput<char>> {
            self.frame(&[key(code, KEY_DOWN)])
        }

        fn release(&mut self, code: u16) -> Vec<HostOutput<char>> {
            self.frame(&[key(code, KEY_UP)])
        }

        /// The full hotkey; returns everything it produced.
        fn hotkey(&mut self) -> Vec<HostOutput<char>> {
            let mut out = vec![];
            for (code, value) in [(META, 1), (GRAVE, 1), (GRAVE, 0), (META, 0)] {
                out.extend(self.frame(&[key(code, value)]));
            }
            out
        }
    }

    /// Strips sequence numbers, for comparing frames sent to clients.
    fn sent(out: &[HostOutput<char>]) -> Vec<(char, Vec<InputEvent>)> {
        out.iter()
            .filter_map(|o| match o {
                Send {
                    peer,
                    message: Message::Frame { events, .. },
                } => Some((*peer, events.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn host_focus_plays_locally() {
        let mut r = Rig::new(&['a']);
        let ev = [
            InputEvent::new(EV_MSC, 4, 0x70004),
            key(A, KEY_DOWN),
            InputEvent::new(EV_SYN, SYN_REPORT, 0),
        ];
        assert_eq!(r.frame(&ev), [Local(ev[..2].to_vec())]);
    }

    #[test]
    fn hotkey_cycles_ready_clients_only() {
        let mut r = Rig::new(&['a', 'b', 'c']);
        r.connect('a');
        r.connect('c');
        // 'b' is up but hasn't acked capabilities: not in the cycle yet.
        r.km.peer_up('b', r.now, &mut vec![]);

        assert_eq!(r.hotkey(), [Focus(Client('a'))]);
        assert_eq!(r.hotkey(), [Focus(Client('c'))]);
        assert_eq!(r.hotkey(), [Focus(Host)]);
        assert_eq!(r.km.ready_clients().collect::<Vec<_>>(), ['a', 'c']);
    }

    #[test]
    fn hotkey_with_no_clients_is_swallowed() {
        let mut r = Rig::new(&['a']);
        assert!(r.hotkey().is_empty());
        assert_eq!(r.km.focus(), Host);
    }

    #[test]
    fn client_focus_sends_numbered_frames() {
        let mut r = Rig::new(&['a']);
        r.connect('a');
        r.hotkey();
        let out = [r.press(A), r.release(A)].concat();
        assert_eq!(
            out,
            [
                Send {
                    peer: 'a',
                    message: Message::Frame {
                        seq: 0,
                        events: vec![key(A, KEY_DOWN)]
                    }
                },
                Send {
                    peer: 'a',
                    message: Message::Frame {
                        seq: 1,
                        events: vec![key(A, KEY_UP)]
                    }
                },
            ]
        );
    }

    #[test]
    fn keys_held_across_a_switch_are_released_on_the_old_target() {
        let mut r = Rig::new(&['a']);
        r.connect('a');
        // Shift held on the host, then switch to 'a'.
        assert_eq!(r.press(SHIFT), [Local(vec![key(SHIFT, KEY_DOWN)])]);
        assert_eq!(
            r.hotkey(),
            [Local(vec![key(SHIFT, KEY_UP)]), Focus(Client('a'))]
        );
        // The rest of Shift's life doesn't leak into 'a'.
        assert!(r.frame(&[key(SHIFT, KEY_REPEAT)]).is_empty());
        assert!(r.release(SHIFT).is_empty());

        // And the other way: held on 'a', released there when leaving.
        r.press(A);
        let out = r.hotkey();
        assert_eq!(sent(&out), [('a', vec![key(A, KEY_UP)])]);
        assert_eq!(out.last(), Some(&Focus(Host)));
        assert!(r.release(A).is_empty());
    }

    #[test]
    fn replayed_key1_is_released_when_hotkey_follows_a_shortcut() {
        // Meta+A, then Meta+Grave while Meta stays down: the replayed Meta
        // press went to the host, so the switch releases it there.
        let mut r = Rig::new(&['a']);
        r.connect('a');
        assert!(r.press(META).is_empty());
        assert_eq!(
            r.press(A),
            [Local(vec![key(META, KEY_DOWN), key(A, KEY_DOWN)])]
        );
        r.release(A);
        r.press(GRAVE);
        r.release(GRAVE);
        assert_eq!(
            r.release(META),
            [Local(vec![key(META, KEY_UP)]), Focus(Client('a'))]
        );
    }

    #[test]
    fn switch_mid_frame_splits_the_frame() {
        let mut r = Rig::new(&['a']);
        r.connect('a');
        r.press(META);
        r.press(GRAVE);
        r.release(GRAVE);
        let mv = InputEvent::new(EV_REL, 0, 3);
        let out = r.frame(&[mv, key(META, KEY_UP), mv]);
        assert_eq!(out[0], Local(vec![mv]));
        assert_eq!(out[1], Focus(Client('a')));
        assert_eq!(sent(&out[2..]), [('a', vec![mv])]);
    }

    #[test]
    fn long_frames_are_chunked() {
        let mut r = Rig::new(&['a']);
        r.connect('a');
        r.hotkey();
        let mv = InputEvent::new(EV_REL, 0, 1);
        let out = r.frame(&vec![mv; MAX_FRAME_EVENTS + 1]);
        let seqs: Vec<(u32, usize)> = out
            .iter()
            .map(|o| match o {
                Send {
                    message: Message::Frame { seq, events },
                    ..
                } => (*seq, events.len()),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(seqs, [(0, MAX_FRAME_EVENTS), (1, 1)]);
    }

    #[test]
    fn focused_client_dropping_returns_focus_to_host() {
        let mut r = Rig::new(&['a', 'b']);
        r.connect('a');
        r.connect('b');
        r.hotkey();
        r.press(A); // held on 'a'
        let mut out = vec![];
        r.km.peer_down('a', &mut out);
        // Nothing is sent to a peer that's gone; it releases its own keys.
        assert_eq!(out, [Focus(Host)]);
        assert_eq!(r.hotkey(), [Focus(Client('b'))]);
        assert_eq!(r.hotkey(), [Focus(Host)]);
    }

    #[test]
    fn unfocused_client_dropping_keeps_focus() {
        let mut r = Rig::new(&['a', 'b']);
        r.connect('a');
        r.connect('b');
        r.hotkey();
        r.hotkey(); // 'b'
        let mut out = vec![];
        r.km.peer_down('a', &mut out);
        assert!(out.is_empty());
        assert_eq!(r.km.focus(), Client('b'));
    }

    #[test]
    fn reconnect_restarts_sequence_and_held_state() {
        let mut r = Rig::new(&['a']);
        r.connect('a');
        r.hotkey();
        r.press(A);
        r.km.peer_down('a', &mut vec![]);
        r.connect('a');
        r.hotkey();
        // A is still physically down; its release doesn't reach the new
        // session, which never saw the press.
        assert!(r.release(A).is_empty());
        let out = r.press(A);
        assert!(matches!(
            &out[..],
            [Send {
                message: Message::Frame { seq: 0, .. },
                ..
            }]
        ));
    }

    #[test]
    fn explicit_focus() {
        let mut r = Rig::new(&['a', 'b']);
        r.connect('b');
        let mut out = vec![];
        assert_eq!(r.km.set_focus(Client('a'), &mut out), Err(Unavailable));
        r.press(SHIFT);
        r.km.set_focus(Client('b'), &mut out).unwrap();
        assert_eq!(out, [Local(vec![key(SHIFT, KEY_UP)]), Focus(Client('b'))]);
        out.clear();
        r.km.set_focus(Client('b'), &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn capabilities_resend_with_backoff_until_acked() {
        let mut r = Rig::new(&['a']);
        let t0 = r.now;
        let mut out = vec![];
        r.km.peer_up('a', t0, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(r.km.next_deadline(), Some(t0 + CAPS_RESEND_INITIAL));

        // Nothing due yet.
        out.clear();
        r.km.poll(t0 + CAPS_RESEND_INITIAL / 2, &mut out);
        assert!(out.is_empty());

        // Resends at 250ms, then 500ms, 1s, 2s, 2s... after each.
        let mut t = t0;
        let mut gaps = vec![];
        while gaps.len() < 6 {
            let due = r.km.next_deadline().unwrap();
            gaps.push(due - t);
            t = due;
            out.clear();
            r.km.poll(t, &mut out);
            assert!(matches!(
                &out[..],
                [Send {
                    message: Message::Capabilities { generation: 1, .. },
                    ..
                }]
            ));
        }
        let ms = |n| Duration::from_millis(n);
        assert_eq!(
            gaps,
            [ms(250), ms(500), ms(1000), ms(2000), ms(2000), ms(2000)]
        );

        r.km.on_message('a', &Message::CapabilitiesAck { generation: 1 })
            .unwrap();
        assert_eq!(r.km.next_deadline(), None);
        assert_eq!(r.km.ready_clients().collect::<Vec<_>>(), ['a']);
    }

    #[test]
    fn peer_down_stops_resends() {
        let mut r = Rig::new(&['a']);
        r.km.peer_up('a', r.now, &mut vec![]);
        r.km.peer_down('a', &mut vec![]);
        assert_eq!(r.km.next_deadline(), None);
        // A late ack from the dead session doesn't make it ready.
        r.km.on_message('a', &Message::CapabilitiesAck { generation: 1 })
            .unwrap();
        assert_eq!(r.km.ready_clients().count(), 0);
    }

    #[test]
    fn changed_capabilities_go_to_connected_clients() {
        let mut r = Rig::new(&['a', 'b']);
        r.connect('a');
        let mut out = vec![];
        r.km.set_capabilities(caps(), r.now, &mut out).unwrap();
        assert!(out.is_empty(), "unchanged capabilities aren't resent");

        let mut bigger = caps();
        bigger.insert(EV_REL, 0).unwrap();
        r.km.set_capabilities(bigger.clone(), r.now, &mut out)
            .unwrap();
        assert_eq!(
            out,
            [Send {
                peer: 'a',
                message: Message::Capabilities {
                    generation: 2,
                    caps: bigger
                }
            }]
        );
        // A late ack for the old generation leaves the new one pending.
        r.km.on_message('a', &Message::CapabilitiesAck { generation: 1 })
            .unwrap();
        assert!(r.km.next_deadline().is_some());
        r.km.on_message('a', &Message::CapabilitiesAck { generation: 2 })
            .unwrap();
        assert_eq!(r.km.next_deadline(), None);
    }

    #[test]
    fn oversized_capabilities_are_rejected() {
        let mut huge = Capabilities::new();
        for code in 0..0x40 {
            huge.insert_abs(code, crate::AbsInfo::default()).unwrap();
        }
        let hotkey = niox_config::DEFAULT_HOTKEY;
        assert!(HostKm::new(hotkey, ['a'], huge.clone()).is_err());
        let mut r = Rig::new(&['a']);
        assert!(r.km.set_capabilities(huge, r.now, &mut vec![]).is_err());
    }

    #[test]
    fn unexpected_messages() {
        let mut r = Rig::new(&['a']);
        let frame = Message::Frame {
            seq: 0,
            events: vec![key(A, 1)],
        };
        assert_eq!(r.km.on_message('a', &frame), Err(UnexpectedMessage));
        let ack = Message::CapabilitiesAck { generation: 1 };
        assert_eq!(r.km.on_message('z', &ack), Err(UnexpectedMessage));
    }

    #[test]
    fn host_and_client_end_to_end() {
        // Everything the host sends, through the wire, into a client.
        let mut r = Rig::new(&['a']);
        let mut client = crate::ClientKm::new();
        let mut out = vec![];
        r.km.peer_up('a', r.now, &mut out);
        let mut played = vec![];
        let mut deliver = |r: &mut Rig, out: Vec<HostOutput<char>>, played: &mut Vec<_>| {
            for o in out {
                let Send { message, .. } = o else { continue };
                let msg = Message::decode(&message.to_bytes().unwrap()).unwrap();
                let reply = client.on_message(msg).unwrap();
                played.extend(reply.play);
                if let Some(ack) = reply.reply {
                    r.km.on_message('a', &ack).unwrap();
                }
            }
        };
        deliver(&mut r, out, &mut played);
        let out = r.hotkey();
        deliver(&mut r, out, &mut played);
        for code in [SHIFT, A] {
            let out = r.press(code);
            deliver(&mut r, out, &mut played);
        }
        let out = r.hotkey();
        deliver(&mut r, out, &mut played);
        assert_eq!(
            played,
            [
                key(SHIFT, KEY_DOWN),
                key(A, KEY_DOWN),
                key(A, KEY_UP),
                key(SHIFT, KEY_UP)
            ]
        );
        assert!(client.host_down().is_empty(), "nothing left held");
    }
}
