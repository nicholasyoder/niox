package transport

import (
	"encoding/binary"
	"errors"
)

// ProtocolVersion is the wire format version stamped on every datagram this
// package sends. A receiver rejects any datagram whose version doesn't
// match.
const ProtocolVersion = 1

// Datagram types, stamped in byte 1 of every datagram after ProtocolVersion.
const (
	typeHandshakeInit     = 1
	typeHandshakeResponse = 2
	typeData              = 3
)

const (
	// headerPrefixSize is the version+type+reserved prefix common to every
	// datagram type.
	headerPrefixSize = 4

	// maxDatagramSize is the largest UDP datagram niox ever sends, chosen as
	// the IPv6 minimum MTU so packets never need fragmentation on a LAN.
	maxDatagramSize = 1280

	dhLen   = 32 // Curve25519 public key size
	tagSize = 16 // ChaCha20-Poly1305 authentication tag

	dataHeaderSize = headerPrefixSize + 4 /* receiver_index */ + 8 /* counter */ // 16

	// MaxPayload is the largest plaintext payload Send accepts.
	MaxPayload = maxDatagramSize - dataHeaderSize - tagSize

	initPayloadSize     = 16 // sender_index(4) + reserved(4) + timestamp(8)
	responsePayloadSize = 8  // sender_index(4) + reserved(4)

	initDatagramSize     = headerPrefixSize + dhLen + initPayloadSize + tagSize     // 68
	responseDatagramSize = headerPrefixSize + dhLen + responsePayloadSize + tagSize // 60
)

// errMalformed indicates a datagram failed structural validation (too
// short, wrong version, unrecognized type) before any crypto was attempted.
var errMalformed = errors.New("transport: malformed datagram")

// dataHeader is the 16-byte header prefixed to every transport-data
// datagram. It is also used verbatim as the AEAD associated data, so every
// field is tamper-evident.
type dataHeader struct {
	receiverIndex uint32 // the index the *peer* announced during handshake
	counter       uint64 // this sender's per-session packet counter; also the AEAD nonce
}

// encode writes h into dst[:dataHeaderSize].
func (h dataHeader) encode(dst []byte) {
	_ = dst[:dataHeaderSize]
	dst[0] = ProtocolVersion
	dst[1] = typeData
	dst[2] = 0
	dst[3] = 0
	binary.BigEndian.PutUint32(dst[4:8], h.receiverIndex)
	binary.BigEndian.PutUint64(dst[8:16], h.counter)
}

// parseDataHeader validates and decodes the header from the start of b. b
// must include at least the trailing AEAD tag, since a data datagram with
// nothing but a header carries no authenticated content.
func parseDataHeader(b []byte) (dataHeader, error) {
	if len(b) < dataHeaderSize+tagSize {
		return dataHeader{}, errMalformed
	}
	if b[0] != ProtocolVersion || b[1] != typeData {
		return dataHeader{}, errMalformed
	}
	return dataHeader{
		receiverIndex: binary.BigEndian.Uint32(b[4:8]),
		counter:       binary.BigEndian.Uint64(b[8:16]),
	}, nil
}

// encodePrefix writes the common version/type/reserved prefix for a
// handshake datagram into dst[:headerPrefixSize].
func encodePrefix(dst []byte, msgType byte) {
	_ = dst[:headerPrefixSize]
	dst[0] = ProtocolVersion
	dst[1] = msgType
	dst[2] = 0
	dst[3] = 0
}

// parsePrefix validates the common prefix and returns the datagram's type
// byte along with the remainder of the datagram after the prefix.
func parsePrefix(b []byte) (msgType byte, rest []byte, err error) {
	if len(b) < headerPrefixSize {
		return 0, nil, errMalformed
	}
	if b[0] != ProtocolVersion {
		return 0, nil, errMalformed
	}
	return b[1], b[headerPrefixSize:], nil
}

// encodeInitPayload builds the plaintext payload carried (encrypted) inside
// a KK handshake message 1: the initiator's session index and a timestamp
// used only as a coarse handshake-replay signal, never as an ordering
// guarantee (see doc.go).
func encodeInitPayload(index uint32, unixNano int64) [initPayloadSize]byte {
	var b [initPayloadSize]byte
	binary.BigEndian.PutUint32(b[0:4], index)
	binary.BigEndian.PutUint64(b[8:16], uint64(unixNano))
	return b
}

func parseInitPayload(b []byte) (index uint32, unixNano int64, err error) {
	if len(b) != initPayloadSize {
		return 0, 0, errMalformed
	}
	index = binary.BigEndian.Uint32(b[0:4])
	unixNano = int64(binary.BigEndian.Uint64(b[8:16]))
	return index, unixNano, nil
}

// encodeResponsePayload builds the plaintext payload carried (encrypted)
// inside a KK handshake message 2: the responder's session index.
func encodeResponsePayload(index uint32) [responsePayloadSize]byte {
	var b [responsePayloadSize]byte
	binary.BigEndian.PutUint32(b[0:4], index)
	return b
}

func parseResponsePayload(b []byte) (index uint32, err error) {
	if len(b) != responsePayloadSize {
		return 0, errMalformed
	}
	return binary.BigEndian.Uint32(b[0:4]), nil
}
