//! The client side of the KM protocol, free of devices and sockets.

use crate::caps::Capabilities;
use crate::event::InputEvent;
use crate::held::HeldKeys;
use crate::wire::Message;

/// What to do in response to one message from the host, in field order:
/// recreate the device, play events, then send the reply.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientOutput {
    /// (Re)create the virtual device with these capabilities. Destroying
    /// the old device releases anything held on it.
    pub recreate: Option<Capabilities>,
    /// Play these events into the device (then `SYN_REPORT`). Empty for
    /// nothing to play.
    pub play: Vec<InputEvent>,
    /// Send this back to the host.
    pub reply: Option<Message>,
}

/// One client's KM state for the host driving it.
///
/// Frames carry a per-session sequence number. Frames normally arrive in
/// order on a LAN, but if one arrives after a newer one (reordered), only
/// its key releases are played: a stale press or motion is dropped, since
/// replaying it late could leave a key stuck down, while a release can only
/// ever un-stick one. Frames that arrive before the device exists are
/// dropped.
#[derive(Debug, Default)]
pub struct ClientKm {
    caps: Option<Capabilities>,
    held: HeldKeys,
    last_seq: Option<u32>,
}

impl ClientKm {
    pub fn new() -> Self {
        Self::default()
    }

    /// The capabilities of the current device, if one has been created.
    pub fn capabilities(&self) -> Option<&Capabilities> {
        self.caps.as_ref()
    }

    /// Handles one message from the host. Returns `None` for a message a
    /// client never expects (an acknowledgment), for the caller to log.
    pub fn on_message(&mut self, message: Message) -> Option<ClientOutput> {
        let mut out = ClientOutput::default();
        match message {
            Message::Capabilities { generation, caps } => {
                out.reply = Some(Message::CapabilitiesAck { generation });
                // Recreate only if the device would gain something, and
                // never drop what it had: devices can come and go on the
                // host, and churning the client's device for that would
                // be disruptive.
                let grown = match &self.caps {
                    Some(have) if have.covers(&caps) => None,
                    Some(have) => {
                        let mut union = have.clone();
                        union.union_with(&caps);
                        Some(union)
                    }
                    None => Some(caps),
                };
                if let Some(caps) = grown {
                    self.held.clear();
                    self.caps = Some(caps.clone());
                    out.recreate = Some(caps);
                }
            }
            Message::Frame { seq, events } => {
                if self.caps.is_none() {
                    return Some(out);
                }
                let fresh = self.last_seq.is_none_or(|last| is_newer(seq, last));
                if fresh {
                    self.last_seq = Some(seq);
                }
                out.play = events
                    .into_iter()
                    .filter(|ev| {
                        if fresh {
                            self.held.filter(ev)
                        } else {
                            self.held.filter_release_only(ev)
                        }
                    })
                    .collect();
            }
            Message::CapabilitiesAck { .. } => return None,
        }
        Some(out)
    }

    /// The session with the host is gone: release everything it left held.
    /// The device stays, ready for the next session.
    pub fn host_down(&mut self) -> Vec<InputEvent> {
        self.last_seq = None;
        self.held.release_all()
    }
}

/// Serial-number comparison (RFC 1982), so sequence numbers can wrap.
fn is_newer(seq: u32, last: u32) -> bool {
    (seq.wrapping_sub(last) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EV_KEY, EV_REL, KEY_DOWN, KEY_UP};

    fn caps(codes: &[u16]) -> Capabilities {
        let mut c = Capabilities::new();
        for &code in codes {
            c.insert(EV_KEY, code).unwrap();
        }
        c
    }

    fn frame(seq: u32, events: &[InputEvent]) -> Message {
        Message::Frame {
            seq,
            events: events.to_vec(),
        }
    }

    fn ready() -> ClientKm {
        let mut c = ClientKm::new();
        c.on_message(Message::Capabilities {
            generation: 1,
            caps: caps(&[30, 31]),
        });
        c
    }

    const A_DOWN: InputEvent = InputEvent::key(30, KEY_DOWN);
    const A_UP: InputEvent = InputEvent::key(30, KEY_UP);
    const MOVE: InputEvent = InputEvent::new(EV_REL, 0, 4);

    #[test]
    fn capabilities_are_acked_and_create_the_device() {
        let mut c = ClientKm::new();
        let out = c
            .on_message(Message::Capabilities {
                generation: 5,
                caps: caps(&[30]),
            })
            .unwrap();
        assert_eq!(out.reply, Some(Message::CapabilitiesAck { generation: 5 }));
        assert_eq!(out.recreate, Some(caps(&[30])));
    }

    #[test]
    fn device_is_recreated_only_when_capabilities_grow() {
        let mut c = ready();
        // A resend, or a subset: acked, no recreate.
        for sub in [caps(&[30, 31]), caps(&[31])] {
            let out = c
                .on_message(Message::Capabilities {
                    generation: 2,
                    caps: sub,
                })
                .unwrap();
            assert_eq!(out.reply, Some(Message::CapabilitiesAck { generation: 2 }));
            assert_eq!(out.recreate, None);
        }
        // Growth: recreated with the union, so nothing is lost.
        let out = c
            .on_message(Message::Capabilities {
                generation: 3,
                caps: caps(&[32]),
            })
            .unwrap();
        assert_eq!(out.recreate, Some(caps(&[30, 31, 32])));
        assert_eq!(c.capabilities(), Some(&caps(&[30, 31, 32])));
    }

    #[test]
    fn frames_before_capabilities_are_dropped() {
        let mut c = ClientKm::new();
        let out = c.on_message(frame(0, &[A_DOWN])).unwrap();
        assert!(out.play.is_empty());
        assert!(c.host_down().is_empty());
    }

    #[test]
    fn plays_frames_and_releases_on_host_down() {
        let mut c = ready();
        assert_eq!(
            c.on_message(frame(0, &[A_DOWN, MOVE])).unwrap().play,
            [A_DOWN, MOVE]
        );
        assert_eq!(c.host_down(), [A_UP]);
        assert!(c.host_down().is_empty());
        // The next session starts its sequence over.
        assert_eq!(c.on_message(frame(0, &[A_DOWN])).unwrap().play, [A_DOWN]);
    }

    #[test]
    fn stale_frames_only_release() {
        let mut c = ready();
        c.on_message(frame(0, &[A_DOWN]));
        // Frame 2 (a press of B) arrives before frame 1 (A up + motion).
        let b_down = InputEvent::key(31, KEY_DOWN);
        assert_eq!(c.on_message(frame(2, &[b_down])).unwrap().play, [b_down]);
        assert_eq!(c.on_message(frame(1, &[A_UP, MOVE])).unwrap().play, [A_UP]);
        // A stale press is dropped rather than left stuck down.
        let mut c = ready();
        c.on_message(frame(5, &[MOVE]));
        assert!(c.on_message(frame(4, &[A_DOWN])).unwrap().play.is_empty());
        assert!(
            c.on_message(frame(5, &[A_DOWN])).unwrap().play.is_empty(),
            "same seq is stale"
        );
        assert!(c.host_down().is_empty());
    }

    #[test]
    fn sequence_wraps() {
        let mut c = ready();
        c.on_message(frame(u32::MAX, &[MOVE]));
        assert_eq!(c.on_message(frame(0, &[A_DOWN])).unwrap().play, [A_DOWN]);
        assert!(is_newer(1, u32::MAX - 5));
        assert!(!is_newer(u32::MAX - 5, 1));
    }

    #[test]
    fn acks_are_unexpected() {
        assert_eq!(
            ready().on_message(Message::CapabilitiesAck { generation: 1 }),
            None
        );
    }
}
