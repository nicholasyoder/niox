package transport

import (
	"crypto/rand"
	"encoding/binary"
	"fmt"
	"time"

	"github.com/flynn/noise"

	"github.com/nicholasyoder/niox/internal/config"
)

// cipherSuite is the fixed Noise cipher suite for niox's transport:
// Curve25519 for DH, ChaCha20-Poly1305 for AEAD (fast on ARM without AES
// hardware, relevant for a headless Raspberry Pi target), BLAKE2s for the
// handshake hash (matching WireGuard's choice, for the same reason).
var cipherSuite = noise.NewCipherSuite(noise.DH25519, noise.CipherChaChaPoly, noise.HashBLAKE2s)

// prologue is mixed into every handshake hash so a peer running an
// incompatible protocol version/pattern fails the handshake cleanly
// instead of misparsing a message it doesn't understand.
var prologue = []byte("niox transport v1")

// defaultHandshakeTimeout is used when Options.HandshakeTimeout is zero.
const defaultHandshakeTimeout = 5 * time.Second

func newHandshakeState(identity config.Identity, peer config.Peer, initiator bool) (*noise.HandshakeState, error) {
	if len(identity.PrivateKey) != dhLen || len(identity.PublicKey) != dhLen {
		return nil, ErrInvalidKey
	}
	if len(peer.PublicKey) != dhLen {
		return nil, ErrInvalidKey
	}
	hs, err := noise.NewHandshakeState(noise.Config{
		CipherSuite: cipherSuite,
		Random:      rand.Reader,
		Pattern:     noise.HandshakeKK,
		Initiator:   initiator,
		Prologue:    prologue,
		StaticKeypair: noise.DHKey{
			Private: identity.PrivateKey,
			Public:  identity.PublicKey,
		},
		PeerStatic: peer.PublicKey,
	})
	if err != nil {
		return nil, fmt.Errorf("transport: create handshake state: %w", err)
	}
	return hs, nil
}

// randomIndex picks this side's session index, announced to the peer
// during the handshake and stamped as receiverIndex on data the peer
// sends back. Zero is reserved as "no session" so is never returned.
func randomIndex() (uint32, error) {
	var b [4]byte
	if _, err := rand.Read(b[:]); err != nil {
		return 0, fmt.Errorf("transport: generate session index: %w", err)
	}
	idx := binary.BigEndian.Uint32(b[:])
	if idx == 0 {
		idx = 1
	}
	return idx, nil
}

// initiate builds the KK message-1 datagram for the initiator side of a
// handshake. The caller must send msg verbatim, and if it needs to retry,
// resend these exact bytes rather than calling initiate again: a second
// call mints a fresh ephemeral keypair and desynchronizes hs from
// anything already sent under the first.
func initiate(identity config.Identity, peer config.Peer) (msg []byte, hs *noise.HandshakeState, localIndex uint32, err error) {
	hs, err = newHandshakeState(identity, peer, true)
	if err != nil {
		return nil, nil, 0, err
	}
	localIndex, err = randomIndex()
	if err != nil {
		return nil, nil, 0, err
	}

	payload := encodeInitPayload(localIndex, time.Now().UnixNano())

	out := make([]byte, headerPrefixSize, initDatagramSize)
	encodePrefix(out, typeHandshakeInit)
	out, _, _, err = hs.WriteMessage(out, payload[:])
	if err != nil {
		return nil, nil, 0, fmt.Errorf("transport: write handshake init: %w", err)
	}
	return out, hs, localIndex, nil
}

// respond processes a received KK message-1 datagram (the full datagram,
// including its version/type prefix) as the responder, and builds the
// message-2 reply datagram plus the resulting session. peerIndex is the
// initiator's announced session index, to be stamped as receiverIndex on
// outgoing data packets.
func respond(msg []byte, identity config.Identity, peer config.Peer) (reply []byte, s *session, peerIndex uint32, err error) {
	msgType, body, err := parsePrefix(msg)
	if err != nil {
		return nil, nil, 0, err
	}
	if msgType != typeHandshakeInit || len(body) != initDatagramSize-headerPrefixSize {
		return nil, nil, 0, errMalformed
	}

	hs, err := newHandshakeState(identity, peer, false)
	if err != nil {
		return nil, nil, 0, err
	}

	payload, _, _, err := hs.ReadMessage(nil, body)
	if err != nil {
		return nil, nil, 0, fmt.Errorf("transport: read handshake init: %w", err)
	}
	peerIndex, _, err = parseInitPayload(payload)
	if err != nil {
		return nil, nil, 0, err
	}

	localIndex, err := randomIndex()
	if err != nil {
		return nil, nil, 0, err
	}
	respPayload := encodeResponsePayload(localIndex)

	out := make([]byte, headerPrefixSize, responseDatagramSize)
	encodePrefix(out, typeHandshakeResponse)
	out, cs1, cs2, err := hs.WriteMessage(out, respPayload[:])
	if err != nil {
		return nil, nil, 0, fmt.Errorf("transport: write handshake response: %w", err)
	}
	if cs1 == nil || cs2 == nil {
		return nil, nil, 0, fmt.Errorf("transport: handshake did not complete after message 2")
	}

	// Split() direction convention (Noise spec, confirmed against
	// flynn/noise's state.go: WriteMessage/ReadMessage both call
	// s.ss.Split() and return its two results unchanged): cs1 is the
	// initiator->responder direction, cs2 is responder->initiator. As the
	// responder, that means we send on cs2 and receive on cs1.
	s = newSession(cs2, cs1, localIndex, peerIndex)
	return out, s, peerIndex, nil
}

// complete processes a received KK message-2 datagram as the initiator,
// finishing the handshake begun by initiate, and returns the resulting
// session. peerIndex is the responder's announced session index.
func complete(hs *noise.HandshakeState, msg []byte, localIndex uint32) (s *session, peerIndex uint32, err error) {
	msgType, body, err := parsePrefix(msg)
	if err != nil {
		return nil, 0, err
	}
	if msgType != typeHandshakeResponse || len(body) != responseDatagramSize-headerPrefixSize {
		return nil, 0, errMalformed
	}

	payload, cs1, cs2, err := hs.ReadMessage(nil, body)
	if err != nil {
		return nil, 0, fmt.Errorf("transport: read handshake response: %w", err)
	}
	if cs1 == nil || cs2 == nil {
		return nil, 0, fmt.Errorf("transport: handshake did not complete after message 2")
	}
	peerIndex, err = parseResponsePayload(payload)
	if err != nil {
		return nil, 0, err
	}

	// As the initiator: send on cs1 (initiator->responder), receive on
	// cs2 (responder->initiator) — see the comment in respond above.
	s = newSession(cs1, cs2, localIndex, peerIndex)
	return s, peerIndex, nil
}
