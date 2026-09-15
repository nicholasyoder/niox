// Wire framing for km payloads carried inside transport.Conn's already
// authenticated/encrypted datagrams (see internal/transport). Events are
// the hot path and use a fixed 9-byte binary encoding; capabilities are
// exchanged once per connection and use JSON since size and frequency
// don't matter there.
package km

import (
	"encoding/binary"
	"encoding/json"
	"fmt"

	"github.com/holoplot/go-evdev"
)

const (
	wireKindCapabilities byte = 1
	wireKindEvent        byte = 2

	// wireEventSize is kind(1) + type(2) + code(2) + value(4).
	wireEventSize = 9
)

// encodeEvent renders ev as a 9-byte wire message. Only Type/Code/Value
// cross the wire -- Time is local capture bookkeeping (used to build
// synthetic key1-press events, see hotkey.go) and has no meaning to the
// receiving side.
func encodeEvent(ev *evdev.InputEvent) []byte {
	b := make([]byte, wireEventSize)
	b[0] = wireKindEvent
	binary.BigEndian.PutUint16(b[1:3], uint16(ev.Type))
	binary.BigEndian.PutUint16(b[3:5], uint16(ev.Code))
	binary.BigEndian.PutUint32(b[5:9], uint32(ev.Value))
	return b
}

// decodeEvent parses a wire message produced by encodeEvent.
func decodeEvent(b []byte) (*evdev.InputEvent, error) {
	if len(b) != wireEventSize || b[0] != wireKindEvent {
		return nil, fmt.Errorf("km: malformed event message (len=%d)", len(b))
	}
	return &evdev.InputEvent{
		Type:  evdev.EvType(binary.BigEndian.Uint16(b[1:3])),
		Code:  evdev.EvCode(binary.BigEndian.Uint16(b[3:5])),
		Value: int32(binary.BigEndian.Uint32(b[5:9])),
	}, nil
}

// encodeCapabilities renders caps as a wire message: a one-byte kind
// prefix followed by a JSON-encoded {EvType: [EvCode, ...]} map.
func encodeCapabilities(caps map[evdev.EvType][]evdev.EvCode) ([]byte, error) {
	body, err := json.Marshal(caps)
	if err != nil {
		return nil, fmt.Errorf("km: encode capabilities: %w", err)
	}
	return append([]byte{wireKindCapabilities}, body...), nil
}

// decodeCapabilities parses a wire message produced by encodeCapabilities.
func decodeCapabilities(b []byte) (map[evdev.EvType][]evdev.EvCode, error) {
	if len(b) < 1 || b[0] != wireKindCapabilities {
		return nil, fmt.Errorf("km: malformed capabilities message (len=%d)", len(b))
	}
	var caps map[evdev.EvType][]evdev.EvCode
	if err := json.Unmarshal(b[1:], &caps); err != nil {
		return nil, fmt.Errorf("km: decode capabilities: %w", err)
	}
	return caps, nil
}

// wireKind returns the message kind byte prefixed to b, or an error if b
// is empty.
func wireKind(b []byte) (byte, error) {
	if len(b) < 1 {
		return 0, fmt.Errorf("km: empty message")
	}
	return b[0], nil
}
