package km

import (
	"testing"

	"github.com/holoplot/go-evdev"
)

func keyEvent(code evdev.EvCode, value int32) *evdev.InputEvent {
	return &evdev.InputEvent{Type: evdev.EV_KEY, Code: code, Value: value}
}

func TestSwitcherNonKeyEventsAlwaysForward(t *testing.T) {
	s := NewSwitcher(evdev.KEY_LEFTMETA, evdev.KEY_GRAVE)

	relEvent := &evdev.InputEvent{Type: evdev.EV_REL, Code: 0, Value: 5}
	forward, synthetic, changed := s.Process(relEvent)
	if !forward || synthetic != nil || changed {
		t.Errorf("non-key event: forward=%v synthetic=%v changed=%v, want true/nil/false", forward, synthetic, changed)
	}
}

func TestSwitcherUnrelatedKeyForwards(t *testing.T) {
	s := NewSwitcher(evdev.KEY_LEFTMETA, evdev.KEY_GRAVE)

	forward, synthetic, changed := s.Process(keyEvent(evdev.KEY_A, keyDown))
	if !forward || synthetic != nil || changed {
		t.Errorf("unrelated key down: forward=%v synthetic=%v changed=%v, want true/nil/false", forward, synthetic, changed)
	}
}

func TestSwitcherSingleKeyHotkeyToggles(t *testing.T) {
	s := NewSwitcher(evdev.KEY_LEFTMETA, 0) // no secondary key.

	if forward, _, changed := s.Process(keyEvent(evdev.KEY_LEFTMETA, keyDown)); forward || changed {
		t.Errorf("key1 down: forward=%v changed=%v, want false/false", forward, changed)
	}
	forward, synthetic, changed := s.Process(keyEvent(evdev.KEY_LEFTMETA, keyUp))
	if forward || synthetic != nil {
		t.Errorf("key1 up: forward=%v synthetic=%v, want false/nil (single-key hotkey has no unrelated-tap case)", forward, synthetic)
	}
	if !changed || s.Focus() != FocusClient {
		t.Errorf("key1 up: changed=%v focus=%v, want true/FocusClient", changed, s.Focus())
	}
}

func TestSwitcherComboHotkeyToggles(t *testing.T) {
	s := NewSwitcher(evdev.KEY_LEFTMETA, evdev.KEY_GRAVE)

	steps := []struct {
		ev            *evdev.InputEvent
		wantForward   bool
		wantChanged   bool
		wantSynthetic bool
	}{
		{keyEvent(evdev.KEY_LEFTMETA, keyDown), false, false, false},
		{keyEvent(evdev.KEY_GRAVE, keyDown), false, false, false},
		{keyEvent(evdev.KEY_GRAVE, keyUp), false, false, false},
		{keyEvent(evdev.KEY_LEFTMETA, keyUp), false, true, false},
	}

	for i, step := range steps {
		forward, synthetic, changed := s.Process(step.ev)
		if forward != step.wantForward || changed != step.wantChanged || (synthetic != nil) != step.wantSynthetic {
			t.Errorf("step %d: forward=%v changed=%v synthetic=%v, want forward=%v changed=%v synthetic!=nil=%v",
				i, forward, changed, synthetic, step.wantForward, step.wantChanged, step.wantSynthetic)
		}
	}
	if s.Focus() != FocusClient {
		t.Errorf("focus after combo = %v, want FocusClient", s.Focus())
	}

	// Repeating the sequence toggles back to the host.
	for _, step := range steps {
		s.Process(step.ev)
	}
	if s.Focus() != FocusHost {
		t.Errorf("focus after second combo = %v, want FocusHost", s.Focus())
	}
}

func TestSwitcherKey1TapWithoutComboReplaysAsNormalKeypress(t *testing.T) {
	s := NewSwitcher(evdev.KEY_LEFTMETA, evdev.KEY_GRAVE)

	// key1 down alone (no key2 involved) is held back...
	if forward, synthetic, changed := s.Process(keyEvent(evdev.KEY_LEFTMETA, keyDown)); forward || synthetic != nil || changed {
		t.Fatalf("key1 down: forward=%v synthetic=%v changed=%v, want false/nil/false", forward, synthetic, changed)
	}

	// ...and replayed (synthetic press + the real release) once it releases
	// without ever forming the combo, so it still works as an ordinary key
	// for whatever's receiving the forwarded stream.
	forward, synthetic, changed := s.Process(keyEvent(evdev.KEY_LEFTMETA, keyUp))
	if !forward || synthetic == nil || changed {
		t.Fatalf("key1 up: forward=%v synthetic=%v changed=%v, want true/non-nil/false", forward, synthetic, changed)
	}
	if synthetic.Code != evdev.KEY_LEFTMETA || synthetic.Value != keyDown {
		t.Errorf("synthetic = %+v, want a key1 down event", synthetic)
	}
	if s.Focus() != FocusHost {
		t.Errorf("focus = %v, want unchanged FocusHost", s.Focus())
	}
}

func TestSwitcherPreventedKey1PressFlushesOnUnrelatedEvent(t *testing.T) {
	s := NewSwitcher(evdev.KEY_LEFTMETA, evdev.KEY_GRAVE)

	if forward, _, _ := s.Process(keyEvent(evdev.KEY_LEFTMETA, keyDown)); forward {
		t.Fatalf("key1 down should be held back")
	}

	// Before key1 releases, an unrelated key arrives (e.g. the user is
	// holding key1 as part of some other combo entirely). The held-back
	// key1 press must be replayed first so it isn't lost.
	forward, synthetic, changed := s.Process(keyEvent(evdev.KEY_A, keyDown))
	if !forward || synthetic == nil || changed {
		t.Fatalf("unrelated event: forward=%v synthetic=%v changed=%v, want true/non-nil/false", forward, synthetic, changed)
	}
	if synthetic.Code != evdev.KEY_LEFTMETA || synthetic.Value != keyDown {
		t.Errorf("synthetic = %+v, want a key1 down event", synthetic)
	}

	// The flush only happens once.
	forward, synthetic, _ = s.Process(keyEvent(evdev.KEY_B, keyDown))
	if !forward || synthetic != nil {
		t.Errorf("second unrelated event: forward=%v synthetic=%v, want true/nil", forward, synthetic)
	}
}

func TestSwitcherKey2WithoutKey1Passes(t *testing.T) {
	s := NewSwitcher(evdev.KEY_LEFTMETA, evdev.KEY_GRAVE)

	// key2 pressed on its own (key1 never down) isn't part of the
	// sequence, so it's just an ordinary keypress.
	forward, synthetic, changed := s.Process(keyEvent(evdev.KEY_GRAVE, keyDown))
	if !forward || synthetic != nil || changed {
		t.Errorf("key2 alone down: forward=%v synthetic=%v changed=%v, want true/nil/false", forward, synthetic, changed)
	}
	forward, synthetic, changed = s.Process(keyEvent(evdev.KEY_GRAVE, keyUp))
	if !forward || synthetic != nil || changed {
		t.Errorf("key2 alone up: forward=%v synthetic=%v changed=%v, want true/nil/false", forward, synthetic, changed)
	}
}
