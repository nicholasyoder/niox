//! End-to-end tests of the socket-free transport: full handshakes and data
//! exchange in memory, and rejection of everything that shouldn't pass.

// Test helpers outside #[test] fns are not covered by clippy.toml.
#![allow(clippy::unwrap_used)]

use niox_config::{PrivateKey, PublicKey};
use niox_transport::{
    DATA_HEADER_LEN, Datagram, Error, INIT_LEN, Initiation, MAX_BODY, MAX_DATAGRAM, MAX_PAYLOAD,
    Payload, RESPONSE_LEN, Session, Subsystem, TAG_LEN, Timestamp, WINDOW_SIZE, classify, initiate,
    respond,
};

struct Peer {
    key: PrivateKey,
}

impl Peer {
    fn new() -> Self {
        Self {
            key: PrivateKey::generate().unwrap(),
        }
    }

    fn public(&self) -> PublicKey {
        self.key.public_key()
    }
}

const A_INDEX: u32 = 0x0A0A_0A0A;
const B_INDEX: u32 = 0x0B0B_0B0B;

/// A initiates to B; returns (A's session, B's session).
fn handshake(a: &Peer, b: &Peer) -> (Session, Session) {
    let mut init = initiate(&a.key, &b.public(), A_INDEX, Timestamp::now()).unwrap();
    let resp = respond(&b.key, &a.public(), init.datagram(), B_INDEX, None).unwrap();
    let a_session = init.complete(&resp.datagram).unwrap();
    (a_session, resp.session)
}

fn seal(session: &Session, payload: Payload<'_>) -> Vec<u8> {
    let mut buf = [0; MAX_DATAGRAM];
    let n = session.seal(payload, &mut buf).unwrap();
    buf[..n].to_vec()
}

/// Opens `datagram` and returns an owned copy of what it carried.
fn open(session: &Session, datagram: &[u8]) -> Result<Option<(Subsystem, Vec<u8>)>, Error> {
    let mut out = [0; MAX_PAYLOAD];
    Ok(match session.open(datagram, &mut out)? {
        Payload::Keepalive => None,
        Payload::Data(subsystem, body) => Some((subsystem, body.to_vec())),
    })
}

#[test]
fn handshake_and_data_both_ways() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    assert!(sa.is_initiator());
    assert!(!sb.is_initiator());
    assert_eq!((sa.local_index(), sa.remote_index()), (A_INDEX, B_INDEX));
    assert_eq!((sb.local_index(), sb.remote_index()), (B_INDEX, A_INDEX));

    let d = seal(&sa, Payload::Data(Subsystem::Km, b"key down"));
    assert_eq!(
        classify(&d).unwrap(),
        Datagram::Data {
            receiver_index: B_INDEX,
            counter: 0
        }
    );
    assert_eq!(
        open(&sb, &d).unwrap(),
        Some((Subsystem::Km, b"key down".to_vec()))
    );

    let d = seal(&sb, Payload::Data(Subsystem::Audio, &[1, 2, 3]));
    assert_eq!(
        open(&sa, &d).unwrap(),
        Some((Subsystem::Audio, vec![1, 2, 3]))
    );

    let d = seal(&sa, Payload::Keepalive);
    assert_eq!(d.len(), DATA_HEADER_LEN + TAG_LEN);
    assert_eq!(open(&sb, &d).unwrap(), None);

    let d = seal(&sb, Payload::Data(Subsystem::Km, &[]));
    assert_eq!(open(&sa, &d).unwrap(), Some((Subsystem::Km, vec![])));
}

#[test]
fn many_datagrams_with_loss_and_reordering() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    let sent: Vec<Vec<u8>> = (0u32..1000)
        .map(|i| seal(&sa, Payload::Data(Subsystem::Km, &i.to_be_bytes())))
        .collect();
    // Deliver in reverse within blocks of 50, dropping every 7th.
    for block in sent.chunks(50) {
        for (i, d) in block.iter().enumerate().rev() {
            if i % 7 == 0 {
                continue;
            }
            assert!(open(&sb, d).unwrap().is_some());
        }
    }
    // A late copy of a dropped datagram (block 950, i = 35) still inside
    // the window is accepted.
    assert!(open(&sb, &sent[985]).unwrap().is_some());
}

#[test]
fn either_side_can_initiate() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sb, sa) = handshake(&b, &a);
    let d = seal(&sa, Payload::Data(Subsystem::Km, b"x"));
    assert!(open(&sb, &d).unwrap().is_some());
}

#[test]
fn largest_payload_fits_the_largest_datagram() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    let body = vec![0x5A; MAX_BODY];
    let d = seal(&sa, Payload::Data(Subsystem::Audio, &body));
    assert_eq!(d.len(), MAX_DATAGRAM);
    assert_eq!(open(&sb, &d).unwrap(), Some((Subsystem::Audio, body)));

    let mut buf = [0; MAX_DATAGRAM + 64];
    assert!(matches!(
        sa.seal(Payload::Data(Subsystem::Km, &[0; MAX_BODY + 1]), &mut buf),
        Err(Error::PayloadTooLarge(n)) if n == MAX_BODY + 1
    ));
}

#[test]
fn buffers_too_small_are_reported_not_truncated() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    let mut small = [0; DATA_HEADER_LEN + TAG_LEN + 3];
    assert!(matches!(
        sa.seal(Payload::Data(Subsystem::Km, b"four"), &mut small),
        Err(Error::BufferTooSmall { need: 37, have: 35 })
    ));

    let d = seal(&sa, Payload::Data(Subsystem::Km, b"four"));
    let mut out = [0; 4];
    assert!(matches!(
        sb.open(&d, &mut out),
        Err(Error::BufferTooSmall { need: 5, have: 4 })
    ));
    // The failed open didn't consume the counter; with room it opens.
    assert!(open(&sb, &d).unwrap().is_some());
}

#[test]
fn duplicate_and_stale_data_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    let first = seal(&sa, Payload::Keepalive);
    open(&sb, &first).unwrap();
    assert!(matches!(open(&sb, &first), Err(Error::Duplicate(0))));

    for _ in 0..WINDOW_SIZE + 1 {
        let d = seal(&sa, Payload::Keepalive);
        open(&sb, &d).unwrap();
    }
    // Counter 0 is now outside the window.
    assert!(matches!(open(&sb, &first), Err(Error::Stale(0))));
}

#[test]
fn every_tampered_data_byte_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    // Keep the original undelivered so a lucky pass couldn't be masked by
    // the replay window, then deliver it last to prove it was intact.
    let d = seal(&sa, Payload::Data(Subsystem::Km, b"payload"));
    for i in 0..d.len() {
        for flip in [0x01, 0x80] {
            let mut t = d.clone();
            t[i] ^= flip;
            assert!(open(&sb, &t).is_err(), "byte {i} ^ {flip:#x} accepted");
        }
    }
    assert!(open(&sb, &d).unwrap().is_some());
}

#[test]
fn truncated_and_extended_data_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    let d = seal(&sa, Payload::Data(Subsystem::Km, b"payload"));
    for len in 0..d.len() {
        assert!(open(&sb, &d[..len]).is_err(), "truncated to {len} accepted");
    }
    let mut longer = d.clone();
    longer.push(0);
    assert!(matches!(open(&sb, &longer), Err(Error::Auth)));
    assert!(open(&sb, &d).unwrap().is_some());
}

#[test]
fn data_from_another_session_is_rejected() {
    let (a, b, c) = (Peer::new(), Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    // A fresh session between the same peers, using the same indices: its
    // keys differ, so neither session opens the other's datagrams.
    let (sa2, sb2) = handshake(&a, &b);
    let d = seal(&sa, Payload::Keepalive);
    assert!(matches!(open(&sb2, &d), Err(Error::Auth)));
    let d = seal(&sa2, Payload::Keepalive);
    assert!(matches!(open(&sb, &d), Err(Error::Auth)));

    // A third machine, even one B also trusts, can't inject into A↔B.
    let (sc, _) = handshake(&c, &b);
    let d = seal(&sc, Payload::Keepalive);
    assert!(matches!(open(&sb, &d), Err(Error::Auth)));
}

#[test]
fn reflected_data_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());

    // With equal indices on both sides, the index check can't catch a
    // datagram bounced back to its sender; the per-direction keys must.
    let mut init = initiate(&a.key, &b.public(), 7, Timestamp::now()).unwrap();
    let resp = respond(&b.key, &a.public(), init.datagram(), 7, None).unwrap();
    let sa = init.complete(&resp.datagram).unwrap();
    let sb = resp.session;

    let d = seal(&sa, Payload::Keepalive);
    assert!(matches!(open(&sa, &d), Err(Error::Auth)));
    let d = seal(&sb, Payload::Keepalive);
    assert!(matches!(open(&sb, &d), Err(Error::Auth)));
}

#[test]
fn misaddressed_data_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);

    let d = seal(&sa, Payload::Keepalive);
    assert!(matches!(
        open(&sa, &d),
        Err(Error::WrongIndex {
            expected: A_INDEX,
            got: B_INDEX
        })
    ));
    let mut init = initiate(&a.key, &b.public(), 1, Timestamp::now()).unwrap();
    assert!(matches!(
        open(&sb, init.datagram()),
        Err(Error::UnexpectedType { .. })
    ));
    assert!(matches!(
        init.complete(&d),
        Err(Error::UnexpectedType { .. })
    ));
}

#[test]
fn unknown_subsystem_is_rejected() {
    // Only reachable by a peer with the session keys, i.e. a newer niox;
    // it's still dropped rather than misread.
    let (a, b) = (Peer::new(), Peer::new());
    let (sa, sb) = handshake(&a, &b);
    let d = seal(&sa, Payload::Data(Subsystem::Km, b"x"));
    assert!(open(&sb, &d).is_ok());
    assert!(matches!(
        Subsystem::try_from(0),
        Err(Error::UnknownSubsystem(0))
    ));
    assert!(matches!(
        Subsystem::try_from(3),
        Err(Error::UnknownSubsystem(3))
    ));
}

// --- handshake rejection ---

fn init_from(a: &Peer, b: &Peer) -> Initiation {
    initiate(&a.key, &b.public(), A_INDEX, Timestamp::now()).unwrap()
}

#[test]
fn handshake_datagram_sizes() {
    let (a, b) = (Peer::new(), Peer::new());
    let init = init_from(&a, &b);
    assert_eq!(init.datagram().len(), INIT_LEN);
    assert_eq!(classify(init.datagram()).unwrap(), Datagram::Init);

    let resp = respond(&b.key, &a.public(), init.datagram(), B_INDEX, None).unwrap();
    assert_eq!(resp.datagram.len(), RESPONSE_LEN);
    assert_eq!(
        classify(&resp.datagram).unwrap(),
        Datagram::Response {
            receiver_index: A_INDEX
        }
    );
}

#[test]
fn init_with_wrong_keys_is_rejected() {
    let (a, b, c) = (Peer::new(), Peer::new(), Peer::new());
    let init = init_from(&a, &b);

    // B expecting C instead of A.
    assert!(matches!(
        respond(&b.key, &c.public(), init.datagram(), B_INDEX, None),
        Err(Error::Auth)
    ));
    // C receiving an init meant for B, even though C trusts A.
    assert!(matches!(
        respond(&c.key, &a.public(), init.datagram(), B_INDEX, None),
        Err(Error::Auth)
    ));
}

#[test]
fn response_with_wrong_keys_is_rejected() {
    let (a, b, c) = (Peer::new(), Peer::new(), Peer::new());
    let mut init = init_from(&a, &b);

    // C intercepts and answers A's init, claiming to be B. C knows A's
    // public key but not B's private key.
    let forged_init = init_from(&a, &c);
    let forged = respond(&c.key, &a.public(), forged_init.datagram(), B_INDEX, None).unwrap();
    assert!(matches!(init.complete(&forged.datagram), Err(Error::Auth)));

    // A response to a different init from A (different ephemeral key).
    let other = init_from(&a, &b);
    let resp = respond(&b.key, &a.public(), other.datagram(), B_INDEX, None).unwrap();
    assert!(matches!(init.complete(&resp.datagram), Err(Error::Auth)));

    // None of that spoiled the pending handshake.
    let resp = respond(&b.key, &a.public(), init.datagram(), B_INDEX, None).unwrap();
    let sa = init.complete(&resp.datagram).unwrap();
    let d = seal(&resp.session, Payload::Keepalive);
    assert_eq!(open(&sa, &d).unwrap(), None);

    assert!(matches!(
        init.complete(&resp.datagram),
        Err(Error::AlreadyComplete)
    ));
}

#[test]
fn every_tampered_init_byte_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());
    let init = init_from(&a, &b);
    let d = init.datagram();
    for i in 0..d.len() {
        let mut t = d.to_vec();
        t[i] ^= 0x01;
        assert!(
            respond(&b.key, &a.public(), &t, B_INDEX, None).is_err(),
            "init byte {i} accepted"
        );
    }
    for len in 0..d.len() {
        assert!(respond(&b.key, &a.public(), &d[..len], B_INDEX, None).is_err());
    }
}

#[test]
fn every_tampered_response_byte_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());
    let mut init = init_from(&a, &b);
    let resp = respond(&b.key, &a.public(), init.datagram(), B_INDEX, None).unwrap();
    let d = resp.datagram;
    for i in 0..d.len() {
        let mut t = d;
        t[i] ^= 0x01;
        assert!(init.complete(&t).is_err(), "response byte {i} accepted");
    }
    for len in 0..d.len() {
        assert!(init.complete(&d[..len]).is_err());
    }
    // Still completes with the genuine response.
    init.complete(&d).unwrap();
}

#[test]
fn replayed_init_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());
    let first = init_from(&a, &b);
    let accepted = respond(&b.key, &a.public(), first.datagram(), B_INDEX, None).unwrap();
    let newest = Some(accepted.timestamp);

    // The same init again (a retransmit or an attacker's replay).
    assert!(matches!(
        respond(&b.key, &a.public(), first.datagram(), B_INDEX, newest),
        Err(Error::ReplayedInit { .. })
    ));

    // An older init that was never answered.
    let old = initiate(&a.key, &b.public(), A_INDEX, Timestamp::from_nanos(1)).unwrap();
    assert!(matches!(
        respond(&b.key, &a.public(), old.datagram(), B_INDEX, newest),
        Err(Error::ReplayedInit { newest: _, got: 1 })
    ));

    // A newer one is fine.
    let later = init_from(&a, &b);
    let resp = respond(&b.key, &a.public(), later.datagram(), B_INDEX, newest).unwrap();
    assert!(resp.timestamp > accepted.timestamp);
}

#[test]
fn handshake_datagram_in_the_wrong_place_is_rejected() {
    let (a, b) = (Peer::new(), Peer::new());
    let mut init = init_from(&a, &b);
    let resp = respond(&b.key, &a.public(), init.datagram(), B_INDEX, None).unwrap();

    assert!(matches!(
        respond(&b.key, &a.public(), &resp.datagram, B_INDEX, None),
        Err(Error::UnexpectedType { .. })
    ));
    let own = init.datagram().to_vec();
    assert!(matches!(
        init.complete(&own),
        Err(Error::UnexpectedType { .. })
    ));
}

#[test]
fn response_for_another_handshake_is_misaddressed() {
    let (a, b) = (Peer::new(), Peer::new());
    let mut init = initiate(&a.key, &b.public(), 1, Timestamp::now()).unwrap();
    let other = initiate(&a.key, &b.public(), 2, Timestamp::now()).unwrap();
    let resp = respond(&b.key, &a.public(), other.datagram(), B_INDEX, None).unwrap();
    assert!(matches!(
        init.complete(&resp.datagram),
        Err(Error::WrongIndex {
            expected: 1,
            got: 2
        })
    ));
}

#[test]
fn errors_display_without_panicking() {
    // Smoke-test a few messages, since they end up in logs.
    let e = Error::WrongIndex {
        expected: 1,
        got: 0xdead_beef,
    };
    assert_eq!(
        e.to_string(),
        "addressed to session index 0xdeadbeef, not 0x00000001"
    );
    assert_eq!(Error::Auth.to_string(), "authentication failed");
}
