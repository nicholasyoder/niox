package km

import (
	"github.com/holoplot/go-evdev"
)

// UinputName is the name niox's virtual keyboard/mouse device reports to
// the kernel, both for the local passthrough device on the host and the
// receiving device on the client. Discover uses it to avoid grabbing our
// own virtual device.
const UinputName = "niox Keyboard and Mouse"

// isKMDevice reports whether dev looks like a keyboard or mouse, matching
// NKMS's heuristic: something with EV_KEY capability that either has a
// left mouse button or a full alphabet key range.
func isKMDevice(dev *evdev.InputDevice) bool {
	name, err := dev.Name()
	if err != nil || name == UinputName {
		return false
	}

	hasKey := false
	for _, t := range dev.CapableTypes() {
		if t == evdev.EV_KEY {
			hasKey = true
			break
		}
	}
	if !hasKey {
		return false
	}

	hasA, hasZ, hasLeftClick := false, false, false
	for _, code := range dev.CapableEvents(evdev.EV_KEY) {
		switch code {
		case evdev.BTN_LEFT:
			hasLeftClick = true
		case evdev.KEY_A:
			hasA = true
		case evdev.KEY_Z:
			hasZ = true
		}
	}
	return hasLeftClick || (hasA && hasZ)
}

// Discover opens every input device under /dev/input that looks like a
// keyboard or mouse (per isKMDevice), skipping niox's own virtual device.
// Callers own the returned devices and must Close each one.
func Discover() ([]*evdev.InputDevice, error) {
	paths, err := evdev.ListDevicePaths()
	if err != nil {
		return nil, err
	}

	var devices []*evdev.InputDevice
	for _, p := range paths {
		dev, err := evdev.Open(p.Path)
		if err != nil {
			continue
		}
		if !isKMDevice(dev) {
			dev.Close()
			continue
		}
		devices = append(devices, dev)
	}
	return devices, nil
}

// Capabilities unions the EV_KEY/EV_REL/... capability code lists of every
// device in devices, matching NKMS's get_capabilities. The result is what
// a Player needs to create a virtual device covering all of them.
func Capabilities(devices []*evdev.InputDevice) map[evdev.EvType][]evdev.EvCode {
	caps := make(map[evdev.EvType][]evdev.EvCode)
	seen := make(map[evdev.EvType]map[evdev.EvCode]bool)

	for _, dev := range devices {
		for _, t := range dev.CapableTypes() {
			if t == evdev.EV_SYN {
				continue
			}
			if seen[t] == nil {
				seen[t] = make(map[evdev.EvCode]bool)
			}
			for _, code := range dev.CapableEvents(t) {
				if seen[t][code] {
					continue
				}
				seen[t][code] = true
				caps[t] = append(caps[t], code)
			}
		}
	}
	return caps
}
