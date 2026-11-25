package km

import (
	"github.com/holoplot/go-evdev"
)

// Focus identifies which machine currently owns the keyboard/mouse.
// Milestone 4 is single-client, so this is a toggle rather than NKMS's
// cycling client index; milestone 5 generalizes it back to N targets.
type Focus int

const (
	FocusHost Focus = iota
	FocusClient
)

// keyEvent value codes, matching Linux's EV_KEY convention (also relied on
// by NKMS's KeyEventType).
const (
	keyUp   int32 = 0
	keyDown int32 = 1
)

// Switcher tracks the hotkey combo's press/release state and decides
// whether an EV_KEY event should be forwarded, and whether it should flip
// Focus. It is a direct port of NKMS's EventHandler key1/key2 state
// machine (see old/nkms/nkms/core/event_handler.py) reduced to a two-way
// toggle. A zero Switcher is not usable; construct with NewSwitcher.
type Switcher struct {
	key1, key2 evdev.EvCode // key2 == 0 means "no secondary key configured"

	focus Focus

	key1Down           bool
	key2Down           bool
	keysActivated      bool
	preventedKey1Press bool
}

// NewSwitcher builds a Switcher for the given hotkey combo. key2 may be 0
// to configure a single-key hotkey (key1 alone).
func NewSwitcher(key1, key2 evdev.EvCode) *Switcher {
	return &Switcher{key1: key1, key2: key2, focus: FocusHost}
}

// Focus returns the currently active target.
func (s *Switcher) Focus() Focus {
	return s.focus
}

func (s *Switcher) toggleFocus() {
	if s.focus == FocusHost {
		s.focus = FocusClient
	} else {
		s.focus = FocusHost
	}
}

// maybeToggle updates key1/key2 down-state for keyCode and toggles focus
// when the configured release sequence completes. Ports
// EventHandler._maybe_get_next_client.
func (s *Switcher) maybeToggle(keyCode evdev.EvCode) (toggled bool) {
	switch keyCode {
	case s.key1:
		if s.key1Down && (s.key2 == 0 || (s.keysActivated && !s.key2Down)) {
			// without key2: key1 was pressed, now releasing.
			// with key2: key1 pressed, key2 pressed, key2 released, key1 now releasing.
			s.toggleFocus()
			s.keysActivated = false
			toggled = true
		}
		s.key1Down = !s.key1Down

	case s.key2:
		if s.key2Down && s.keysActivated && !s.key1Down {
			// key1 pressed, key2 pressed, key1 released, now key2 releasing.
			s.toggleFocus()
			s.keysActivated = false
			toggled = true
		}
		if s.key1Down {
			s.keysActivated = true
		}
		s.key2Down = !s.key2Down
	}
	return toggled
}

// makeKey1Press builds a synthetic key1-down event to immediately precede
// ev, used to replay a key1 press that was held back while niox was still
// deciding whether it was part of the hotkey combo. Ports
// EventHandler._make_key1_press_from_event.
func (s *Switcher) makeKey1Press(ev *evdev.InputEvent) *evdev.InputEvent {
	t := ev.Time
	if t.Usec > 0 {
		t.Usec--
	}
	return &evdev.InputEvent{
		Time:  t,
		Type:  evdev.EV_KEY,
		Code:  s.key1,
		Value: keyDown,
	}
}

// Process decides what to do with one captured input event. It returns:
//   - forward: whether ev itself should be delivered (locally or remotely).
//   - synthetic: a key1-down event to deliver immediately before ev, if
//     niox had been holding back key1's press waiting to see whether it
//     was part of the hotkey combo (nil otherwise).
//   - changed: whether this call toggled Focus (the new value is in
//     s.Focus()).
//
// Ports EventHandler._process_event.
func (s *Switcher) Process(ev *evdev.InputEvent) (forward bool, synthetic *evdev.InputEvent, changed bool) {
	if ev.Type != evdev.EV_KEY || (ev.Value != keyUp && ev.Value != keyDown) {
		return true, nil, false // only key up/down events get special handling.
	}

	if ev.Code == s.key1 || (s.key2 != 0 && ev.Code == s.key2) {
		s.preventedKey1Press = false
		toggled := s.maybeToggle(ev.Code)

		switch {
		case ev.Value == keyUp && ev.Code == s.key1 && !toggled && !s.key2Down:
			// key1 up without being part of our sequence: let the release
			// through, but first replay the press we held back earlier.
			return true, s.makeKey1Press(ev), toggled

		case ev.Code == s.key2 && !toggled && !s.key1Down && !s.keysActivated:
			// key2 up/down without key1 having been pressed first: not our
			// sequence, let it through untouched.
			return true, nil, toggled

		default:
			if ev.Code == s.key1 && ev.Value == keyDown {
				s.preventedKey1Press = true
			}
			return false, nil, toggled
		}
	}

	if s.preventedKey1Press {
		// We held back a key1 press; this unrelated event means it wasn't
		// part of a hotkey sequence after all, so replay it now.
		s.preventedKey1Press = false
		return true, s.makeKey1Press(ev), false
	}

	return true, nil, false
}
