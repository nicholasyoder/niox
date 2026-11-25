//! The Noise KK handshake: [`initiate`], [`respond`], and
//! [`Initiation::complete`].

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use niox_config::{PrivateKey, PublicKey};
use snow::{Builder, HandshakeState};

use crate::Error;
use crate::packet::{
    self, Datagram, INDEX_LEN, INIT_LEN, INIT_PAYLOAD_LEN, PREFIX_LEN, RESPONSE_LEN,
    RESPONSE_PAYLOAD_LEN, TYPE_INIT, TYPE_RESPONSE,
};
use crate::session::Session;

/// Curve25519 for DH, ChaCha20-Poly1305 (fast without AES hardware, e.g.
/// on a Raspberry Pi host), and BLAKE2s, as in WireGuard.
const PATTERN: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";

/// Mixed into every handshake hash, so a peer speaking a different
/// protocol version fails the handshake instead of misreading it.
const PROLOGUE: &[u8] = b"niox transport v1";

/// When a handshake init was created: nanoseconds since the Unix epoch.
///
/// A responder only accepts an init newer than the last one it accepted
/// from the same peer, so a recorded init can't be replayed later. This
/// relies on the initiator's wall clock not stepping backwards across a
/// restart; if it does, its handshakes are refused until the clock passes
/// the old value again (the same tradeoff WireGuard makes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The current time, strictly greater than any earlier `now()` in this
    /// process, so two inits created in quick succession never tie.
    pub fn now() -> Self {
        static LAST: AtomicU64 = AtomicU64::new(0);
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        let mut last = LAST.load(Ordering::Relaxed);
        loop {
            let next = wall.max(last.saturating_add(1));
            match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Self(next),
                Err(actual) => last = actual,
            }
        }
    }

    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub const fn as_nanos(self) -> u64 {
        self.0
    }
}

fn builder<'a>(local: &'a PrivateKey, peer: &'a PublicKey) -> Result<Builder<'a>, Error> {
    let params = PATTERN.parse()?;
    Ok(Builder::new(params)
        .local_private_key(local.as_bytes())?
        .remote_public_key(peer.as_bytes())?
        .prologue(PROLOGUE)?)
}

/// A handshake this side started, waiting for the peer's response.
pub struct Initiation {
    /// `None` once the handshake has completed.
    state: Option<HandshakeState>,
    local_index: u32,
    datagram: [u8; INIT_LEN],
}

impl fmt::Debug for Initiation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Initiation")
            .field("local_index", &self.local_index)
            .field("complete", &self.state.is_none())
            .finish_non_exhaustive()
    }
}

/// Starts a handshake with `peer`.
///
/// `local_index` is how the peer will address the resulting session (and
/// the response to this init); the caller picks one not already in use.
/// `timestamp` is normally [`Timestamp::now`].
///
/// To retransmit, resend [`Initiation::datagram`] unchanged. Calling
/// `initiate` again starts a separate handshake with a new ephemeral key.
pub fn initiate(
    local: &PrivateKey,
    peer: &PublicKey,
    local_index: u32,
    timestamp: Timestamp,
) -> Result<Initiation, Error> {
    let mut state = builder(local, peer)?.build_initiator()?;

    let mut payload = [0; INIT_PAYLOAD_LEN];
    payload[..INDEX_LEN].copy_from_slice(&local_index.to_be_bytes());
    payload[INDEX_LEN..].copy_from_slice(&timestamp.0.to_be_bytes());

    let mut datagram = [0; INIT_LEN];
    packet::write_prefix(&mut datagram, TYPE_INIT);
    let written = state.write_message(&payload, &mut datagram[PREFIX_LEN..])?;
    debug_assert_eq!(PREFIX_LEN + written, INIT_LEN);

    Ok(Initiation {
        state: Some(state),
        local_index,
        datagram,
    })
}

impl Initiation {
    /// The init datagram to send to the peer.
    pub fn datagram(&self) -> &[u8] {
        &self.datagram
    }

    pub fn local_index(&self) -> u32 {
        self.local_index
    }

    /// Finishes the handshake with the peer's response.
    ///
    /// A response that fails to verify leaves the initiation untouched, so
    /// a forged or corrupted datagram can't spoil the real response still
    /// on its way. After success, further calls return
    /// [`Error::AlreadyComplete`].
    pub fn complete(&mut self, response: &[u8]) -> Result<Session, Error> {
        let Datagram::Response { receiver_index } = packet::classify(response)? else {
            return Err(Error::UnexpectedType {
                expected: "handshake response",
            });
        };
        if receiver_index != self.local_index {
            return Err(Error::WrongIndex {
                expected: self.local_index,
                got: receiver_index,
            });
        }
        let state = self.state.as_mut().ok_or(Error::AlreadyComplete)?;

        let mut payload = [0; RESPONSE_PAYLOAD_LEN];
        let read = state.read_message(&response[PREFIX_LEN + INDEX_LEN..], &mut payload)?;
        if read != RESPONSE_PAYLOAD_LEN {
            return Err(Error::Malformed("wrong handshake response payload length"));
        }
        let remote_index = packet::read_u32(&payload);

        let state = self.state.take().ok_or(Error::AlreadyComplete)?;
        Session::new(state, self.local_index, remote_index)
    }
}

/// The responder's side of a completed handshake.
#[derive(Debug)]
pub struct Responded {
    /// The response datagram to send back to the initiator.
    pub datagram: [u8; RESPONSE_LEN],
    pub session: Session,
    /// The init's timestamp. Once the caller commits to this session, it
    /// passes this as `newest_seen` the next time it responds to the same
    /// peer.
    pub timestamp: Timestamp,
}

/// Answers a handshake `init` that claims to come from `peer`.
///
/// `newest_seen` is the timestamp of the last init accepted from `peer`
/// (`None` if there wasn't one); an init that isn't newer is rejected as a
/// replay. An init from any other peer, or encrypted to any other key,
/// fails with [`Error::Auth`].
///
/// A retransmitted init is also rejected as a replay once the first copy
/// was accepted. The caller should resend its earlier response instead.
pub fn respond(
    local: &PrivateKey,
    peer: &PublicKey,
    init: &[u8],
    local_index: u32,
    newest_seen: Option<Timestamp>,
) -> Result<Responded, Error> {
    if packet::classify(init)? != Datagram::Init {
        return Err(Error::UnexpectedType {
            expected: "handshake init",
        });
    }
    let mut state = builder(local, peer)?.build_responder()?;

    let mut payload = [0; INIT_PAYLOAD_LEN];
    let read = state.read_message(&init[PREFIX_LEN..], &mut payload)?;
    if read != INIT_PAYLOAD_LEN {
        return Err(Error::Malformed("wrong handshake init payload length"));
    }
    let remote_index = packet::read_u32(&payload);
    let timestamp = Timestamp(packet::read_u64(&payload[INDEX_LEN..]));
    if let Some(newest) = newest_seen
        && timestamp <= newest
    {
        return Err(Error::ReplayedInit {
            newest: newest.0,
            got: timestamp.0,
        });
    }

    let mut datagram = [0; RESPONSE_LEN];
    packet::write_prefix(&mut datagram, TYPE_RESPONSE);
    datagram[PREFIX_LEN..PREFIX_LEN + INDEX_LEN].copy_from_slice(&remote_index.to_be_bytes());
    let written = state.write_message(
        &local_index.to_be_bytes(),
        &mut datagram[PREFIX_LEN + INDEX_LEN..],
    )?;
    debug_assert_eq!(PREFIX_LEN + INDEX_LEN + written, RESPONSE_LEN);

    Ok(Responded {
        datagram,
        session: Session::new(state, local_index, remote_index)?,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_strictly_increase() {
        let mut last = Timestamp::now();
        for _ in 0..10_000 {
            let next = Timestamp::now();
            assert!(next > last);
            last = next;
        }
    }

    #[test]
    fn timestamps_track_the_wall_clock() {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let now = u128::from(Timestamp::now().as_nanos());
        assert!(now.abs_diff(wall) < 5_000_000_000);
    }
}
