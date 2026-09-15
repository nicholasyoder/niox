package transport

import (
	"errors"
	"sync/atomic"

	"github.com/flynn/noise"
)

var (
	errReplayed = errors.New("transport: duplicate packet counter")
	errStale    = errors.New("transport: stale packet counter")
)

// dropReason classifies why session.open rejected a datagram, so Conn can
// attribute it to the matching Stats counter without re-deriving the
// reason.
type dropReason int

const (
	dropNone dropReason = iota
	dropMalformed
	dropAuth
	dropReplay
	dropStale
)

// session holds one established, direction-mapped pair of ciphers plus the
// per-direction state (send counter, receive replay window) needed to seal
// and open transport-data datagrams. It has no knowledge of sockets.
//
// seal is safe for concurrent use: the send counter is atomic and the
// underlying AEAD cipher has no shared mutable state across calls with
// distinct nonces. open is not: a session has exactly one receive path
// (Conn.Recv is documented single-reader), so the replay window needs no
// locking.
type session struct {
	sendCipher noise.Cipher
	recvCipher noise.Cipher

	localIndex  uint32
	remoteIndex uint32

	next   atomic.Uint64
	window replayWindow
}

// newSession captures the stateless Cipher for each direction via
// CipherState.Cipher() once, up front. After this call the source
// *noise.CipherStates are invalid (flynn/noise poisons them on Cipher())
// and must not be touched again — session owns nonce management from here
// on, which is required since UDP can deliver datagrams out of order.
func newSession(send, recv *noise.CipherState, localIndex, remoteIndex uint32) *session {
	return &session{
		sendCipher:  send.Cipher(),
		recvCipher:  recv.Cipher(),
		localIndex:  localIndex,
		remoteIndex: remoteIndex,
	}
}

// seal encrypts payload into a new transport-data datagram addressed to
// the peer's announced session index. The returned slice is a fresh
// allocation, safe to hand off after seal returns.
func (s *session) seal(payload []byte) ([]byte, error) {
	if len(payload) > MaxPayload {
		return nil, ErrPayloadTooLarge
	}
	n := s.next.Add(1) - 1
	if n >= rejectAfterCounter {
		return nil, ErrSessionExpired
	}

	out := make([]byte, dataHeaderSize, dataHeaderSize+len(payload)+tagSize)
	dataHeader{receiverIndex: s.remoteIndex, counter: n}.encode(out)
	out = s.sendCipher.Encrypt(out, n, out[:dataHeaderSize], payload)
	return out, nil
}

// open validates and decrypts a received transport-data datagram,
// appending the plaintext to buf. It authenticates before consulting the
// replay window (see doc.go): a forged datagram with an arbitrary counter
// must never be able to move the window and cause later genuine datagrams
// to be rejected as stale.
func (s *session) open(datagram []byte, buf []byte) (plaintext []byte, reason dropReason, err error) {
	header, err := parseDataHeader(datagram)
	if err != nil {
		return nil, dropMalformed, err
	}
	if header.receiverIndex != s.localIndex {
		return nil, dropMalformed, errMalformed
	}

	// Check the buffer has room before decrypting: Decrypt (via AEAD.Open)
	// silently falls back to a fresh allocation when buf's capacity is
	// too small, which would return plaintext the caller's buffer never
	// actually received. The datagram is still consumed in this case —
	// there is no way to put it back once read from the socket.
	ciphertext := datagram[dataHeaderSize:]
	if len(ciphertext)-tagSize > cap(buf)-len(buf) {
		return nil, dropNone, ErrPayloadTooLarge
	}

	plaintext, err = s.recvCipher.Decrypt(buf, header.counter, datagram[:dataHeaderSize], ciphertext)
	if err != nil {
		return nil, dropAuth, err
	}

	switch s.window.accept(header.counter) {
	case replayDuplicate:
		return nil, dropReplay, errReplayed
	case replayStale:
		return nil, dropStale, errStale
	}
	return plaintext, dropNone, nil
}
