//! KM messages: the body of every `Subsystem::Km` transport payload.
//!
//! All integers are big-endian. The first byte is the message kind.
//!
//! | kind      | layout after the kind byte                                    |
//! |-----------|---------------------------------------------------------------|
//! | 1 frame   | seq (4), then 1..=[`MAX_FRAME_EVENTS`] × { type (2), code (2), value (4) } |
//! | 2 caps    | generation (4), capabilities (below)                          |
//! | 3 caps ack| generation (4)                                                |
//!
//! A frame is one evdev frame: the events between two `SYN_REPORT`s, not
//! including the `SYN_REPORT` itself, which the receiver appends when it
//! plays the frame. `seq` counts frames per session (see
//! [`crate::ClientKm`] for how stale frames are handled).
//!
//! Capabilities are encoded as bitmaps, the way the kernel stores them,
//! so even a full keyboard's key set takes under 100 bytes:
//!
//! - properties: a 4-byte bitmap (bit *n* = `INPUT_PROP` *n*);
//! - a type count (1), then per type, ascending: type (1), bitmap length
//!   in bytes (1), the bitmap (bit *n* of byte *n*/8, least significant
//!   first, is code *n*);
//! - for each `EV_ABS` code, ascending: minimum, maximum, fuzz, flat,
//!   resolution (4 each).

use crate::caps::{ABS_LIMIT, AbsInfo, CODE_LIMIT, Capabilities, PROP_LIMIT, TYPE_LIMIT};
use crate::event::{EV_ABS, InputEvent};

/// The largest KM message, so it fits one transport datagram.
pub const MAX_MESSAGE: usize = niox_transport::MAX_BODY;

const FRAME_HEADER: usize = 1 + 4;
const EVENT_LEN: usize = 8;

/// The most events one frame message holds. Frames longer than this
/// (rare: heavy multitouch) are split into several messages.
pub const MAX_FRAME_EVENTS: usize = (MAX_MESSAGE - FRAME_HEADER) / EVENT_LEN;

const KIND_FRAME: u8 = 1;
const KIND_CAPS: u8 = 2;
const KIND_CAPS_ACK: u8 = 3;

/// A KM message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Host → client: one frame of input to play.
    Frame { seq: u32, events: Vec<InputEvent> },
    /// Host → client: the capabilities the client's virtual device needs.
    /// Resent until acknowledged.
    Capabilities { generation: u32, caps: Capabilities },
    /// Client → host: acknowledges [`Message::Capabilities`].
    CapabilitiesAck { generation: u32 },
}

/// Why a message couldn't be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("empty message")]
    Empty,
    #[error("unknown message kind {0}")]
    UnknownKind(u8),
    #[error("malformed {0} message")]
    Malformed(&'static str),
    #[error("frame has {0} events; the limit is 1..={MAX_FRAME_EVENTS}")]
    FrameSize(usize),
    #[error("message is {0} bytes; the limit is {MAX_MESSAGE}")]
    TooLarge(usize),
}

impl Message {
    /// Appends the encoded message to `buf`.
    pub fn encode(&self, buf: &mut Vec<u8>) -> Result<(), WireError> {
        let start = buf.len();
        match self {
            Message::Frame { seq, events } => {
                if events.is_empty() || events.len() > MAX_FRAME_EVENTS {
                    return Err(WireError::FrameSize(events.len()));
                }
                buf.push(KIND_FRAME);
                buf.extend_from_slice(&seq.to_be_bytes());
                for ev in events {
                    buf.extend_from_slice(&ev.kind.to_be_bytes());
                    buf.extend_from_slice(&ev.code.to_be_bytes());
                    buf.extend_from_slice(&ev.value.to_be_bytes());
                }
            }
            Message::Capabilities { generation, caps } => {
                buf.push(KIND_CAPS);
                buf.extend_from_slice(&generation.to_be_bytes());
                encode_caps(caps, buf);
            }
            Message::CapabilitiesAck { generation } => {
                buf.push(KIND_CAPS_ACK);
                buf.extend_from_slice(&generation.to_be_bytes());
            }
        }
        let len = buf.len() - start;
        if len > MAX_MESSAGE {
            buf.truncate(start);
            return Err(WireError::TooLarge(len));
        }
        Ok(())
    }

    /// Encodes the message into a new buffer.
    pub fn to_bytes(&self) -> Result<Vec<u8>, WireError> {
        let mut buf = Vec::new();
        self.encode(&mut buf)?;
        Ok(buf)
    }

    /// Parses a message. Anything malformed, including trailing bytes, is
    /// rejected.
    pub fn decode(b: &[u8]) -> Result<Message, WireError> {
        let (&kind, rest) = b.split_first().ok_or(WireError::Empty)?;
        let mut r = Reader(rest);
        let msg = match kind {
            KIND_FRAME => {
                let seq = r.u32().ok_or(WireError::Malformed("frame"))?;
                let body = r.0;
                if body.len() % EVENT_LEN != 0 {
                    return Err(WireError::Malformed("frame"));
                }
                let n = body.len() / EVENT_LEN;
                if n == 0 || n > MAX_FRAME_EVENTS {
                    return Err(WireError::FrameSize(n));
                }
                let mut events = Vec::with_capacity(n);
                for _ in 0..n {
                    let (Some(kind), Some(code), Some(value)) = (r.u16(), r.u16(), r.i32()) else {
                        return Err(WireError::Malformed("frame"));
                    };
                    let ev = InputEvent { kind, code, value };
                    if ev.is_frame_boundary() {
                        return Err(WireError::Malformed("frame"));
                    }
                    events.push(ev);
                }
                Message::Frame { seq, events }
            }
            KIND_CAPS => {
                let generation = r.u32().ok_or(WireError::Malformed("capabilities"))?;
                let caps = decode_caps(&mut r).ok_or(WireError::Malformed("capabilities"))?;
                Message::Capabilities { generation, caps }
            }
            KIND_CAPS_ACK => {
                let generation = r.u32().ok_or(WireError::Malformed("capabilities ack"))?;
                Message::CapabilitiesAck { generation }
            }
            other => return Err(WireError::UnknownKind(other)),
        };
        if !r.0.is_empty() {
            return Err(WireError::Malformed(match msg {
                Message::Frame { .. } => "frame",
                Message::Capabilities { .. } => "capabilities",
                Message::CapabilitiesAck { .. } => "capabilities ack",
            }));
        }
        Ok(msg)
    }
}

fn bitmap(codes: impl Iterator<Item = u16>) -> Vec<u8> {
    let mut bits = Vec::new();
    for code in codes {
        let byte = usize::from(code / 8);
        if bits.len() <= byte {
            bits.resize(byte + 1, 0);
        }
        bits[byte] |= 1 << (code % 8);
    }
    bits
}

fn bits(bitmap: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bitmap.iter().enumerate().flat_map(|(i, &byte)| {
        (0..8u16)
            .filter(move |bit| byte & (1 << bit) != 0)
            .map(move |bit| i as u16 * 8 + bit)
    })
}

fn encode_caps(caps: &Capabilities, buf: &mut Vec<u8>) {
    let mut props = [0u8; 4];
    let p = bitmap(caps.properties());
    props[..p.len()].copy_from_slice(&p);
    buf.extend_from_slice(&props);

    // Capabilities bounds types below 0x20 and codes below 0x300, so the
    // count and every bitmap length fit a byte.
    let types: Vec<_> = caps.types().collect();
    buf.push(types.len() as u8);
    for (kind, codes) in types {
        let map = bitmap(codes.iter().copied());
        buf.push(kind as u8);
        buf.push(map.len() as u8);
        buf.extend_from_slice(&map);
    }
    for (_, info) in caps.abs() {
        for v in [
            info.minimum,
            info.maximum,
            info.fuzz,
            info.flat,
            info.resolution,
        ] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }
}

fn decode_caps(r: &mut Reader<'_>) -> Option<Capabilities> {
    let mut caps = Capabilities::new();
    for prop in bits(r.take(4)?) {
        debug_assert!(prop < PROP_LIMIT);
        caps.insert_property(prop).ok()?;
    }
    let count = r.u8()?;
    let mut prev = None;
    let mut abs_codes = Vec::new();
    for _ in 0..count {
        let kind = u16::from(r.u8()?);
        if prev.is_some_and(|p| p >= kind) || kind >= TYPE_LIMIT {
            return None; // types must be unique and ascending
        }
        prev = Some(kind);
        let len = usize::from(r.u8()?);
        let limit = if kind == EV_ABS {
            ABS_LIMIT
        } else {
            CODE_LIMIT
        };
        if len > usize::from(limit / 8) {
            return None;
        }
        let map = r.take(len)?;
        caps.insert_type(kind).ok()?;
        if kind == EV_ABS {
            abs_codes.extend(bits(map));
        } else {
            for code in bits(map) {
                caps.insert(kind, code).ok()?;
            }
        }
    }
    for code in abs_codes {
        let info = AbsInfo {
            minimum: r.i32()?,
            maximum: r.i32()?,
            fuzz: r.i32()?,
            flat: r.i32()?,
            resolution: r.i32()?,
        };
        caps.insert_abs(code, info).ok()?;
    }
    Some(caps)
}

/// Encoded size of a capabilities message, to check before sending.
pub fn capabilities_len(caps: &Capabilities) -> usize {
    let mut buf = Vec::new();
    encode_caps(caps, &mut buf);
    1 + 4 + buf.len()
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2)?.try_into().ok().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)?.try_into().ok().map(u32::from_be_bytes)
    }

    fn i32(&mut self) -> Option<i32> {
        self.take(4)?.try_into().ok().map(i32::from_be_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EV_KEY, EV_MSC, EV_REL, EV_SYN, SYN_DROPPED, SYN_REPORT};

    const EV_REP: u16 = 0x14;

    fn full_caps() -> Capabilities {
        let mut c = Capabilities::new();
        for code in 1..0x2ff {
            c.insert(EV_KEY, code).unwrap();
        }
        c.insert(EV_REL, 0).unwrap();
        c.insert(EV_REL, 1).unwrap();
        c.insert(EV_REL, 8).unwrap();
        c.insert(EV_MSC, 4).unwrap();
        c.insert_type(EV_REP).unwrap();
        for code in [0, 1, 0x2f, 0x35, 0x36, 0x39] {
            c.insert_abs(
                code,
                AbsInfo {
                    minimum: -5,
                    maximum: 1000 + i32::from(code),
                    fuzz: 1,
                    flat: 2,
                    resolution: 12,
                },
            )
            .unwrap();
        }
        c.insert_property(0).unwrap();
        c.insert_property(2).unwrap();
        c
    }

    fn round_trip(msg: Message) {
        let bytes = msg.to_bytes().unwrap();
        assert!(bytes.len() <= MAX_MESSAGE);
        assert_eq!(Message::decode(&bytes).unwrap(), msg);
        // Every truncation fails, and so does a trailing byte. (A frame cut
        // at an event boundary is just a shorter frame.)
        let is_frame = matches!(msg, Message::Frame { .. });
        for n in 0..bytes.len() {
            if is_frame && n > FRAME_HEADER && (n - FRAME_HEADER).is_multiple_of(EVENT_LEN) {
                continue;
            }
            assert!(Message::decode(&bytes[..n]).is_err(), "truncated to {n}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(Message::decode(&long).is_err());
    }

    #[test]
    fn frame_round_trip() {
        round_trip(Message::Frame {
            seq: 0xdead_beef,
            events: vec![
                InputEvent::new(EV_MSC, 4, 0x700e3),
                InputEvent::key(125, 1),
                InputEvent::new(EV_REL, 0, -42),
                InputEvent::new(EV_SYN, 2, 0), // SYN_MT_REPORT is content
            ],
        });
        let max = Message::Frame {
            seq: 1,
            events: vec![InputEvent::new(EV_REL, 0, i32::MIN); MAX_FRAME_EVENTS],
        };
        assert!(max.to_bytes().unwrap().len() <= MAX_MESSAGE);
        round_trip(max);
    }

    #[test]
    fn frame_limits() {
        let empty = Message::Frame {
            seq: 0,
            events: vec![],
        };
        assert_eq!(empty.to_bytes(), Err(WireError::FrameSize(0)));
        let over = Message::Frame {
            seq: 0,
            events: vec![InputEvent::key(30, 1); MAX_FRAME_EVENTS + 1],
        };
        assert_eq!(
            over.to_bytes(),
            Err(WireError::FrameSize(MAX_FRAME_EVENTS + 1))
        );
        assert_eq!(
            Message::decode(&[KIND_FRAME, 0, 0, 0, 1]),
            Err(WireError::FrameSize(0))
        );
    }

    #[test]
    fn frame_rejects_boundaries() {
        for code in [SYN_REPORT, SYN_DROPPED] {
            let mut bytes = vec![KIND_FRAME, 0, 0, 0, 1];
            bytes.extend_from_slice(&EV_SYN.to_be_bytes());
            bytes.extend_from_slice(&code.to_be_bytes());
            bytes.extend_from_slice(&0i32.to_be_bytes());
            assert_eq!(Message::decode(&bytes), Err(WireError::Malformed("frame")));
        }
    }

    #[test]
    fn caps_round_trip() {
        round_trip(Message::Capabilities {
            generation: 7,
            caps: Capabilities::new(),
        });
        let caps = full_caps();
        let msg = Message::Capabilities {
            generation: u32::MAX,
            caps: caps.clone(),
        };
        assert_eq!(msg.to_bytes().unwrap().len(), capabilities_len(&caps));
        round_trip(msg);
    }

    #[test]
    fn caps_too_large() {
        let mut caps = Capabilities::new();
        for code in 0..ABS_LIMIT {
            caps.insert_abs(code, AbsInfo::default()).unwrap();
        }
        let msg = Message::Capabilities {
            generation: 1,
            caps: caps.clone(),
        };
        let mut buf = vec![9];
        assert_eq!(
            msg.encode(&mut buf),
            Err(WireError::TooLarge(capabilities_len(&caps)))
        );
        assert_eq!(buf, [9], "a failed encode leaves the buffer as it was");
    }

    #[test]
    fn caps_rejects_bad_encodings() {
        let header = [KIND_CAPS, 0, 0, 0, 1, 0, 0, 0, 0];
        let decode = |tail: &[u8]| Message::decode(&[&header[..], tail].concat());

        assert!(decode(&[0]).is_ok());
        // Out-of-order and duplicate types.
        assert!(decode(&[2, 2, 1, 1, 1, 1, 1]).is_err());
        assert!(decode(&[2, 1, 1, 1, 1, 1, 1]).is_err());
        // EV_SYN, and a type past EV_MAX.
        assert!(decode(&[1, 0, 1, 1]).is_err());
        assert!(decode(&[1, 0x20, 0]).is_err());
        // A key bitmap longer than KEY_CNT, and an ABS bitmap past ABS_CNT.
        assert!(decode(&[&[1, 1, 97][..], &[0; 97]].concat()).is_err());
        assert!(decode(&[&[1, 3, 9][..], &[0; 9]].concat()).is_err());
        // An ABS axis missing its range.
        assert!(decode(&[1, 3, 1, 1]).is_err());
    }

    #[test]
    fn ack_round_trip_and_unknown_kind() {
        round_trip(Message::CapabilitiesAck { generation: 3 });
        assert_eq!(Message::decode(&[]), Err(WireError::Empty));
        assert_eq!(Message::decode(&[9]), Err(WireError::UnknownKind(9)));
    }
}
