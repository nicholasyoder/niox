//! niox's transport: Noise-authenticated, AEAD-encrypted datagrams between
//! two configured peers, shared by the KM and (later) audio subsystems. See
//! ARCHITECTURE.md.
//!
//! The crate has two layers. The socket-free core ([`initiate`],
//! [`respond`], [`Session`]) turns keys and bytes into datagrams and back,
//! and never touches the network or the clock (apart from
//! [`Timestamp::now`]), so every path is unit-testable. [`Endpoint`] is the
//! socket half built on it: one UDP socket serving every configured peer,
//! with handshake retransmission, keepalives, liveness detection, and
//! automatic re-handshakes (see its module docs for the lifecycle).
//!
//! # Handshake
//!
//! `Noise_KK_25519_ChaChaPoly_BLAKE2s`: both sides already know each
//! other's static key from config, so neither key ever goes on the wire.
//! Either peer may initiate:
//!
//! 1. The initiator calls [`initiate`] and sends [`Initiation::datagram`].
//!    Its encrypted payload carries the initiator's session index and a
//!    [`Timestamp`].
//! 2. The responder calls [`respond`], which yields its [`Session`] and a
//!    response datagram carrying the responder's session index. It rejects
//!    an init whose timestamp isn't newer than the last one it accepted
//!    from that peer, so a captured init can't be replayed to knock a live
//!    session over.
//! 3. The initiator calls [`Initiation::complete`] with the response and
//!    gets its [`Session`].
//!
//! # Data
//!
//! Each data datagram carries the receiver's session index (so the socket
//! layer can find the session with a lookup, without trial decryption) and
//! an explicit 64-bit counter used as the AEAD nonce. Datagrams can be lost
//! or reordered and each one still decrypts on its own. A sliding window
//! rejects duplicates and anything too old. Ordering is *not* restored: a
//! subsystem that needs it handles it itself.
//!
//! Every data payload begins with a [`Subsystem`] byte, so all subsystems
//! share one session per peer. An empty payload is a keepalive.
//!
//! # Wire format
//!
//! All integers are big-endian. Every datagram starts with a 4-byte prefix:
//! protocol version, datagram type, and two reserved bytes that must be
//! zero. No datagram exceeds [`MAX_DATAGRAM`] bytes (the IPv6 minimum MTU,
//! so nothing is ever fragmented).
//!
//! | type         | layout after the prefix                                           | size |
//! |--------------|-------------------------------------------------------------------|------|
//! | 1 init       | ephemeral (32), sealed { sender index (4), timestamp (8) }        | 64   |
//! | 2 response   | receiver index (4), ephemeral (32), sealed { sender index (4) }   | 60   |
//! | 3 data       | receiver index (4), counter (8), sealed { subsystem (1), body }   | ≥ 32 |
//!
//! "sealed" includes the 16-byte Poly1305 tag. The cleartext header fields
//! aren't passed to the AEAD as associated data (snow's stateless transport
//! doesn't support it), but, as in WireGuard, none of them can be altered
//! undetected: the counter is the nonce, a changed index selects a session
//! or handshake whose keys won't open the datagram, the version is also
//! bound into every handshake through the Noise prologue, and the type and
//! reserved bytes are checked exactly.

mod endpoint;
mod handshake;
mod packet;
mod replay;
mod session;

pub use endpoint::{
    Endpoint, EndpointConfig, Event, Handle, PeerError, PeerId, PeerSpec, SendError, Timers,
};
pub use handshake::{Initiation, Responded, Timestamp, initiate, respond};
pub use packet::{
    DATA_HEADER_LEN, Datagram, INIT_LEN, MAX_BODY, MAX_DATAGRAM, MAX_PAYLOAD, PROTOCOL_VERSION,
    RESPONSE_LEN, TAG_LEN, classify,
};
pub use replay::{REJECT_AFTER_COUNTER, WINDOW_SIZE};
pub use session::{Payload, Session, Subsystem};

/// Why a datagram was rejected or couldn't be built.
///
/// For received datagrams, every variant means "drop it". They're distinct
/// so the socket layer can count and log drops by cause; none of them
/// should be relayed to the peer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Wrong length or a nonzero reserved byte.
    #[error("malformed datagram: {0}")]
    Malformed(&'static str),
    #[error("unsupported protocol version {0}")]
    Version(u8),
    #[error("unknown datagram type {0}")]
    UnknownType(u8),
    /// The datagram was valid but of a type the caller didn't expect here
    /// (e.g. a data datagram handed to [`respond`]).
    #[error("expected a {expected} datagram")]
    UnexpectedType { expected: &'static str },
    /// Addressed to a different session or pending handshake.
    #[error("addressed to session index {got:#010x}, not {expected:#010x}")]
    WrongIndex { expected: u32, got: u32 },
    /// The AEAD tag didn't verify: tampered, forged, from a different
    /// session, or encrypted with keys other than the configured ones.
    #[error("authentication failed")]
    Auth,
    /// A handshake init whose timestamp isn't newer than the last one
    /// accepted from the same peer.
    #[error("handshake init timestamp {got} is not newer than {newest} (replayed init?)")]
    ReplayedInit { newest: u64, got: u64 },
    #[error("duplicate packet counter {0}")]
    Duplicate(u64),
    /// Too far behind the newest counter seen, or past
    /// [`REJECT_AFTER_COUNTER`].
    #[error("stale packet counter {0}")]
    Stale(u64),
    #[error("unknown subsystem {0}")]
    UnknownSubsystem(u8),
    #[error("payload of {0} bytes exceeds the {MAX_BODY}-byte maximum")]
    PayloadTooLarge(usize),
    #[error("output buffer of {have} bytes is too small; need {need}")]
    BufferTooSmall { need: usize, have: usize },
    /// This session has sent [`REJECT_AFTER_COUNTER`] packets and must be
    /// replaced by a new handshake.
    #[error("session send counter exhausted; a new handshake is required")]
    Exhausted,
    #[error("handshake already completed")]
    AlreadyComplete,
    /// Any other failure inside the Noise implementation (e.g. the random
    /// source failing while generating an ephemeral key).
    #[error("noise protocol error")]
    Noise(#[source] snow::Error),
}

impl From<snow::Error> for Error {
    fn from(err: snow::Error) -> Self {
        match err {
            snow::Error::Decrypt => Self::Auth,
            other => Self::Noise(other),
        }
    }
}
