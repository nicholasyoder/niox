// Package transport provides the Noise-handshake-authenticated,
// AEAD-encrypted UDP transport shared by the km and audio subsystems for
// host<->client network traffic. See ARCHITECTURE.md.
//
// The wire protocol is Noise_KK_25519_ChaChaPoly_BLAKE2s: both peers'
// static public keys are already known from config before any packet is
// sent (see internal/config), so KK's pre-shared-static-keys handshake is
// used rather than a pattern that transmits an identity on the wire.
//
// A Conn guarantees authenticity and freshness — every delivered payload
// really came from the configured peer, and stale or duplicate datagrams
// are silently dropped rather than delivered — but it does not guarantee
// ordering. A reordered-but-fresh datagram is delivered in arrival order,
// not send order, matching the "a dropped or stale mouse-move should
// never be retransmitted" principle in ARCHITECTURE.md. A subsystem that
// needs strict ordering (e.g. a key-down/key-up pair) must carry its own
// sequence number in its payload; the transport's internal packet counter
// is an anti-replay mechanism, not an application sequence number, and it
// resets on every new handshake.
//
// This package is deliberately spike-scoped (ROADMAP.md milestone 2):
// handshake retransmission, handshake-level replay protection, and
// session roaming/rehandshake-in-place after a peer's address changes are
// not implemented. Dial and Accept each attempt exactly one handshake,
// bounded by Options.HandshakeTimeout. These are revisited once the km
// subsystem (milestone 4) exercises this package under real conditions.
package transport
