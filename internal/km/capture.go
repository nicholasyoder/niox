package km

import (
	"context"
	"fmt"

	"github.com/holoplot/go-evdev"
)

// CaptureLoop grabs dev for exclusive access (see ARCHITECTURE.md: this is
// why host-focused events still need local replay through a Player) and
// delivers every event it reads to out, until ctx is done or the device
// disappears. It owns dev's lifecycle: Ungrab and Close happen here,
// including on error, mirroring NKMS's handle_events "always release the
// device" behavior (old/nkms/nkms/core/event_handler.py).
//
// dev must support NonBlock (see its doc comment) so that closing it from
// another goroutine interrupts a pending read -- that's how ctx
// cancellation unblocks the read loop below.
func CaptureLoop(ctx context.Context, dev *evdev.InputDevice, out chan<- *evdev.InputEvent) error {
	if err := dev.Grab(); err != nil {
		return fmt.Errorf("km: grab %s: %w", dev.Path(), err)
	}
	defer func() {
		_ = dev.Ungrab()
		_ = dev.Close()
	}()

	if err := dev.NonBlock(); err != nil {
		return fmt.Errorf("km: set nonblock %s: %w", dev.Path(), err)
	}

	stop := make(chan struct{})
	defer close(stop)
	go func() {
		select {
		case <-ctx.Done():
			_ = dev.Close() // unblocks a pending ReadOne below.
		case <-stop:
		}
	}()

	for {
		ev, err := dev.ReadOne()
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			return fmt.Errorf("km: read %s: %w", dev.Path(), err)
		}

		select {
		case out <- ev:
		case <-ctx.Done():
			return nil
		}
	}
}
