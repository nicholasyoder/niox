//! Datagram layout: the common prefix, sizes, and [`classify`]. The wire
//! format table is in the crate docs.

use crate::Error;

/// Stamped on every datagram. A receiver drops any other version.
pub const PROTOCOL_VERSION: u8 = 1;

/// The largest datagram niox sends or accepts: the IPv6 minimum MTU, so
/// datagrams are never fragmented.
pub const MAX_DATAGRAM: usize = 1280;

/// Poly1305 authentication tag.
pub const TAG_LEN: usize = 16;

pub(crate) const PREFIX_LEN: usize = 4;
pub(crate) const INDEX_LEN: usize = 4;
pub(crate) const KEY_LEN: usize = 32;
pub(crate) const TIMESTAMP_LEN: usize = 8;

pub(crate) const TYPE_INIT: u8 = 1;
pub(crate) const TYPE_RESPONSE: u8 = 2;
pub(crate) const TYPE_DATA: u8 = 3;

/// Init plaintext: sender index + timestamp.
pub(crate) const INIT_PAYLOAD_LEN: usize = INDEX_LEN + TIMESTAMP_LEN;
/// Response plaintext: sender index.
pub(crate) const RESPONSE_PAYLOAD_LEN: usize = INDEX_LEN;

/// A handshake init datagram's exact size.
pub const INIT_LEN: usize = PREFIX_LEN + KEY_LEN + INIT_PAYLOAD_LEN + TAG_LEN;
/// A handshake response datagram's exact size.
pub const RESPONSE_LEN: usize = PREFIX_LEN + INDEX_LEN + KEY_LEN + RESPONSE_PAYLOAD_LEN + TAG_LEN;
/// Prefix, receiver index, and counter in front of a data datagram's
/// ciphertext.
pub const DATA_HEADER_LEN: usize = PREFIX_LEN + INDEX_LEN + 8;
/// The largest data plaintext: subsystem byte plus body.
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - DATA_HEADER_LEN - TAG_LEN;
/// The largest body a subsystem can send in one datagram.
pub const MAX_BODY: usize = MAX_PAYLOAD - 1;

/// What a received datagram is, from its cleartext fields alone. Nothing
/// has been authenticated yet; this is only for routing a datagram to the
/// handshake or session that can verify it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datagram {
    /// Handshake init, for [`crate::respond`]. Which peer sent it is only
    /// known once one peer's key decrypts it.
    Init,
    /// Handshake response, for the pending [`crate::Initiation`] whose
    /// local index is `receiver_index`.
    Response { receiver_index: u32 },
    /// Data, for the [`crate::Session`] whose local index is
    /// `receiver_index`.
    Data { receiver_index: u32, counter: u64 },
}

/// Checks a received datagram's structure (version, type, reserved bytes,
/// length) and reads its routing fields.
pub fn classify(datagram: &[u8]) -> Result<Datagram, Error> {
    if datagram.len() > MAX_DATAGRAM {
        return Err(Error::Malformed("longer than the maximum datagram"));
    }
    let [version, kind, r0, r1, rest @ ..] = datagram else {
        return Err(Error::Malformed("shorter than the datagram prefix"));
    };
    if *version != PROTOCOL_VERSION {
        return Err(Error::Version(*version));
    }
    if (*r0, *r1) != (0, 0) {
        return Err(Error::Malformed("nonzero reserved byte"));
    }
    match *kind {
        TYPE_INIT if datagram.len() == INIT_LEN => Ok(Datagram::Init),
        TYPE_INIT => Err(Error::Malformed("wrong handshake init length")),
        TYPE_RESPONSE if datagram.len() == RESPONSE_LEN => Ok(Datagram::Response {
            receiver_index: read_u32(rest),
        }),
        TYPE_RESPONSE => Err(Error::Malformed("wrong handshake response length")),
        TYPE_DATA if datagram.len() >= DATA_HEADER_LEN + TAG_LEN => Ok(Datagram::Data {
            receiver_index: read_u32(rest),
            counter: read_u64(&rest[INDEX_LEN..]),
        }),
        TYPE_DATA => Err(Error::Malformed("data datagram too short")),
        other => Err(Error::UnknownType(other)),
    }
}

/// Writes the common prefix into `out[..PREFIX_LEN]`.
pub(crate) fn write_prefix(out: &mut [u8], kind: u8) {
    out[..PREFIX_LEN].copy_from_slice(&[PROTOCOL_VERSION, kind, 0, 0]);
}

/// Reads a big-endian u32 from the start of `b`, which callers have
/// already length-checked.
pub(crate) fn read_u32(b: &[u8]) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&b[..4]);
    u32::from_be_bytes(bytes)
}

pub(crate) fn read_u64(b: &[u8]) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&b[..8]);
    u64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(kind: u8, len: usize) -> Vec<u8> {
        let mut d = vec![0xAA; len];
        write_prefix(&mut d, kind);
        d
    }

    #[test]
    fn sizes_match_the_documented_layout() {
        assert_eq!(INIT_LEN, 64);
        assert_eq!(RESPONSE_LEN, 60);
        assert_eq!(DATA_HEADER_LEN, 16);
        assert_eq!(MAX_PAYLOAD, 1248);
        assert_eq!(MAX_BODY, 1247);
    }

    #[test]
    fn classifies_each_type() {
        assert_eq!(
            classify(&datagram(TYPE_INIT, INIT_LEN)).unwrap(),
            Datagram::Init
        );

        let mut resp = datagram(TYPE_RESPONSE, RESPONSE_LEN);
        resp[4..8].copy_from_slice(&0x0102_0304u32.to_be_bytes());
        assert_eq!(
            classify(&resp).unwrap(),
            Datagram::Response {
                receiver_index: 0x0102_0304
            }
        );

        let mut data = datagram(TYPE_DATA, DATA_HEADER_LEN + TAG_LEN + 3);
        data[4..8].copy_from_slice(&7u32.to_be_bytes());
        data[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(
            classify(&data).unwrap(),
            Datagram::Data {
                receiver_index: 7,
                counter: 0x1122_3344_5566_7788
            }
        );
    }

    #[test]
    fn data_length_bounds() {
        // An empty payload (keepalive) is the shortest valid data datagram.
        assert!(classify(&datagram(TYPE_DATA, DATA_HEADER_LEN + TAG_LEN)).is_ok());
        assert!(matches!(
            classify(&datagram(TYPE_DATA, DATA_HEADER_LEN + TAG_LEN - 1)),
            Err(Error::Malformed(_))
        ));
        assert!(classify(&datagram(TYPE_DATA, MAX_DATAGRAM)).is_ok());
        assert!(matches!(
            classify(&datagram(TYPE_DATA, MAX_DATAGRAM + 1)),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn handshake_lengths_are_exact() {
        for len in [INIT_LEN - 1, INIT_LEN + 1] {
            assert!(matches!(
                classify(&datagram(TYPE_INIT, len)),
                Err(Error::Malformed(_))
            ));
        }
        for len in [RESPONSE_LEN - 1, RESPONSE_LEN + 1] {
            assert!(matches!(
                classify(&datagram(TYPE_RESPONSE, len)),
                Err(Error::Malformed(_))
            ));
        }
    }

    #[test]
    fn rejects_bad_prefix() {
        for len in 0..PREFIX_LEN {
            assert!(matches!(
                classify(&datagram(TYPE_DATA, 64)[..len]),
                Err(Error::Malformed(_))
            ));
        }

        let mut d = datagram(TYPE_INIT, INIT_LEN);
        d[0] = 2;
        assert!(matches!(classify(&d), Err(Error::Version(2))));

        for reserved in [2, 3] {
            let mut d = datagram(TYPE_INIT, INIT_LEN);
            d[reserved] = 1;
            assert!(matches!(classify(&d), Err(Error::Malformed(_))));
        }

        for kind in [0, 4, 255] {
            assert!(matches!(
                classify(&datagram(kind, 64)),
                Err(Error::UnknownType(k)) if k == kind
            ));
        }
    }
}
