//! An established session: sealing and opening data datagrams.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use snow::{HandshakeState, StatelessTransportState};

use crate::Error;
use crate::packet::{self, DATA_HEADER_LEN, Datagram, MAX_BODY, MAX_PAYLOAD, TAG_LEN, TYPE_DATA};
use crate::replay::{REJECT_AFTER_COUNTER, ReplayWindow};

/// Which subsystem a data payload belongs to: the first plaintext byte of
/// every non-empty data payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum Subsystem {
    Km = 1,
    Audio = 2,
}

impl TryFrom<u8> for Subsystem {
    type Error = Error;

    fn try_from(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::Km),
            2 => Ok(Self::Audio),
            other => Err(Error::UnknownSubsystem(other)),
        }
    }
}

/// The plaintext of a data datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload<'a> {
    /// An empty payload. Proves the session is alive and carries nothing.
    Keepalive,
    /// A subsystem's message, at most [`MAX_BODY`] bytes.
    Data(Subsystem, &'a [u8]),
}

/// One established session with a peer: two keys (one per direction), the
/// indices each side uses to address it, the send counter, and the receive
/// replay window. It knows nothing about sockets.
///
/// `Session` is `Sync`: any number of threads can [`seal`](Self::seal) and
/// [`open`](Self::open) through a shared reference. Sending only bumps an
/// atomic counter; receiving takes a short lock on the replay window.
#[derive(Debug)]
pub struct Session {
    transport: StatelessTransportState,
    local_index: u32,
    remote_index: u32,
    next_counter: AtomicU64,
    window: Mutex<ReplayWindow>,
}

impl Session {
    pub(crate) fn new(
        state: HandshakeState,
        local_index: u32,
        remote_index: u32,
    ) -> Result<Self, Error> {
        Ok(Self {
            transport: state.into_stateless_transport_mode()?,
            local_index,
            remote_index,
            next_counter: AtomicU64::new(0),
            window: Mutex::new(ReplayWindow::default()),
        })
    }

    /// The index the peer puts on datagrams addressed to this session.
    pub fn local_index(&self) -> u32 {
        self.local_index
    }

    /// The index this session puts on datagrams to the peer.
    pub fn remote_index(&self) -> u32 {
        self.remote_index
    }

    /// Whether this side sent the handshake init.
    pub fn is_initiator(&self) -> bool {
        self.transport.is_initiator()
    }

    /// Encrypts `payload` into a data datagram in `out`, returning its
    /// length. `out` needs room for [`DATA_HEADER_LEN`] + payload +
    /// [`TAG_LEN`] bytes; [`crate::MAX_DATAGRAM`] always suffices.
    pub fn seal(&self, payload: Payload<'_>, out: &mut [u8]) -> Result<usize, Error> {
        let mut plaintext = [0; MAX_PAYLOAD];
        let len = match payload {
            Payload::Keepalive => 0,
            Payload::Data(_, body) if body.len() > MAX_BODY => {
                return Err(Error::PayloadTooLarge(body.len()));
            }
            Payload::Data(subsystem, body) => {
                plaintext[0] = subsystem as u8;
                plaintext[1..=body.len()].copy_from_slice(body);
                body.len() + 1
            }
        };
        let need = DATA_HEADER_LEN + len + TAG_LEN;
        if out.len() < need {
            return Err(Error::BufferTooSmall {
                need,
                have: out.len(),
            });
        }

        let counter = self.next_counter.fetch_add(1, Ordering::Relaxed);
        if counter >= REJECT_AFTER_COUNTER {
            // Pin the counter so it can't creep toward wrapping however
            // long a caller keeps trying.
            self.next_counter
                .store(REJECT_AFTER_COUNTER, Ordering::Relaxed);
            return Err(Error::Exhausted);
        }

        packet::write_prefix(out, TYPE_DATA);
        out[4..8].copy_from_slice(&self.remote_index.to_be_bytes());
        out[8..DATA_HEADER_LEN].copy_from_slice(&counter.to_be_bytes());
        let written = self.transport.write_message(
            counter,
            &plaintext[..len],
            &mut out[DATA_HEADER_LEN..need],
        )?;
        debug_assert_eq!(DATA_HEADER_LEN + written, need);
        Ok(need)
    }

    /// Verifies and decrypts a data datagram addressed to this session,
    /// decrypting into `out` (which needs room for the datagram's length
    /// minus [`DATA_HEADER_LEN`] + [`TAG_LEN`]; [`MAX_PAYLOAD`] always
    /// suffices).
    ///
    /// The datagram is authenticated before its counter is recorded, so a
    /// forged datagram can never move the replay window.
    pub fn open<'a>(&self, datagram: &[u8], out: &'a mut [u8]) -> Result<Payload<'a>, Error> {
        let Datagram::Data {
            receiver_index,
            counter,
        } = packet::classify(datagram)?
        else {
            return Err(Error::UnexpectedType { expected: "data" });
        };
        if receiver_index != self.local_index {
            return Err(Error::WrongIndex {
                expected: self.local_index,
                got: receiver_index,
            });
        }
        let ciphertext = &datagram[DATA_HEADER_LEN..];
        let need = ciphertext.len() - TAG_LEN;
        if out.len() < need {
            return Err(Error::BufferTooSmall {
                need,
                have: out.len(),
            });
        }

        // Cheap early drop for duplicates and stale counters; the real
        // check-and-record happens after authentication.
        self.lock_window().check(counter)?;
        let len = self.transport.read_message(counter, ciphertext, out)?;
        self.lock_window().accept(counter)?;

        match &out[..len] {
            [] => Ok(Payload::Keepalive),
            [subsystem, body @ ..] => Ok(Payload::Data(Subsystem::try_from(*subsystem)?, body)),
        }
    }

    fn lock_window(&self) -> std::sync::MutexGuard<'_, ReplayWindow> {
        // The window is consistent after every statement, so a panic
        // elsewhere while it was locked leaves nothing to repair.
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAX_DATAGRAM, Timestamp, initiate, respond};
    use niox_config::PrivateKey;

    fn pair() -> (Session, Session) {
        let a = PrivateKey::generate().unwrap();
        let b = PrivateKey::generate().unwrap();
        let mut init = initiate(&a, &b.public_key(), 1, Timestamp::now()).unwrap();
        let resp = respond(&b, &a.public_key(), init.datagram(), 2, None).unwrap();
        (init.complete(&resp.datagram).unwrap(), resp.session)
    }

    #[test]
    fn send_counter_exhaustion() {
        let (a, b) = pair();
        let mut buf = [0; MAX_DATAGRAM];
        let mut out = [0; MAX_PAYLOAD];

        a.next_counter
            .store(REJECT_AFTER_COUNTER - 1, Ordering::Relaxed);
        let n = a.seal(Payload::Keepalive, &mut buf).unwrap();
        assert_eq!(b.open(&buf[..n], &mut out).unwrap(), Payload::Keepalive);

        for _ in 0..3 {
            assert!(matches!(
                a.seal(Payload::Keepalive, &mut buf),
                Err(Error::Exhausted)
            ));
        }
        assert_eq!(a.next_counter.load(Ordering::Relaxed), REJECT_AFTER_COUNTER);
    }
}
