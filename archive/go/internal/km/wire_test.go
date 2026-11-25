package km

import (
	"testing"

	"github.com/holoplot/go-evdev"
)

func TestEncodeDecodeEventRoundTrip(t *testing.T) {
	want := &evdev.InputEvent{Type: evdev.EV_KEY, Code: evdev.KEY_A, Value: 1}

	got, err := decodeEvent(encodeEvent(want))
	if err != nil {
		t.Fatalf("decodeEvent: %v", err)
	}
	if got.Type != want.Type || got.Code != want.Code || got.Value != want.Value {
		t.Errorf("got %+v, want %+v", got, want)
	}
}

func TestEncodeDecodeEventNegativeValue(t *testing.T) {
	// EV_REL mouse-move deltas can be negative.
	want := &evdev.InputEvent{Type: evdev.EV_REL, Code: 0, Value: -37}

	got, err := decodeEvent(encodeEvent(want))
	if err != nil {
		t.Fatalf("decodeEvent: %v", err)
	}
	if got.Value != want.Value {
		t.Errorf("value = %d, want %d", got.Value, want.Value)
	}
}

func TestDecodeEventRejectsMalformed(t *testing.T) {
	cases := [][]byte{
		nil,
		{wireKindEvent},
		{wireKindCapabilities, 0, 0, 0, 0, 0, 0, 0, 0}, // right size, wrong kind.
	}
	for _, c := range cases {
		if _, err := decodeEvent(c); err == nil {
			t.Errorf("decodeEvent(%v): expected error, got nil", c)
		}
	}
}

func TestEncodeDecodeCapabilitiesRoundTrip(t *testing.T) {
	want := map[evdev.EvType][]evdev.EvCode{
		evdev.EV_KEY: {evdev.KEY_A, evdev.KEY_Z, evdev.BTN_LEFT},
		evdev.EV_REL: {0, 1},
	}

	msg, err := encodeCapabilities(want)
	if err != nil {
		t.Fatalf("encodeCapabilities: %v", err)
	}
	got, err := decodeCapabilities(msg)
	if err != nil {
		t.Fatalf("decodeCapabilities: %v", err)
	}

	if len(got) != len(want) {
		t.Fatalf("got %d event types, want %d", len(got), len(want))
	}
	for evType, codes := range want {
		gotCodes, ok := got[evType]
		if !ok || len(gotCodes) != len(codes) {
			t.Errorf("codes for %v = %v, want %v", evType, gotCodes, codes)
			continue
		}
		for i := range codes {
			if gotCodes[i] != codes[i] {
				t.Errorf("codes[%v][%d] = %d, want %d", evType, i, gotCodes[i], codes[i])
			}
		}
	}
}

func TestDecodeCapabilitiesRejectsWrongKind(t *testing.T) {
	msg := encodeEvent(&evdev.InputEvent{Type: evdev.EV_KEY, Code: evdev.KEY_A, Value: 1})
	if _, err := decodeCapabilities(msg); err == nil {
		t.Error("decodeCapabilities: expected error for an event-kind message, got nil")
	}
}

func TestWireKind(t *testing.T) {
	kind, err := wireKind([]byte{wireKindEvent, 1, 2})
	if err != nil || kind != wireKindEvent {
		t.Errorf("wireKind = %d, %v, want %d, nil", kind, err, wireKindEvent)
	}
	if _, err := wireKind(nil); err == nil {
		t.Error("wireKind(nil): expected error, got nil")
	}
}
