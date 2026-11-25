package km

import (
	"github.com/holoplot/go-evdev"
)

// virtualID is the InputID niox reports for its own uinput devices.
// BusType/Vendor/Product/Version are arbitrary but stable, matching what
// python-evdev's UInput uses by convention for a synthetic device.
var virtualID = evdev.InputID{BusType: evdev.BUS_VIRTUAL, Vendor: 1, Product: 1, Version: 1}

// Player is a local uinput device that replays captured input events --
// either the host's own passthrough device (see ARCHITECTURE.md: devices
// are grabbed exclusively to intercept the hotkey, so even host-focused
// events must be re-injected) or a client's receiving device.
type Player struct {
	dev *evdev.InputDevice
}

// NewPlayer creates a uinput device covering caps.
func NewPlayer(caps map[evdev.EvType][]evdev.EvCode) (*Player, error) {
	dev, err := evdev.CreateDevice(UinputName, virtualID, caps)
	if err != nil {
		return nil, err
	}
	return &Player{dev: dev}, nil
}

// Write replays ev and flushes it with a SYN_REPORT, matching NKMS's
// write-then-always-sync pattern (see old/nkms/nkms/core/virtual_input_linux.py).
// This is harmless even when ev is itself a SYN event, since NKMS forwards
// those too.
func (p *Player) Write(ev *evdev.InputEvent) error {
	if err := p.dev.WriteOne(ev); err != nil {
		return err
	}
	return p.dev.WriteOne(&evdev.InputEvent{Type: evdev.EV_SYN, Code: evdev.SYN_REPORT, Value: 0})
}

// Close destroys the uinput device.
func (p *Player) Close() error {
	return evdev.DestroyDevice(p.dev)
}
