package km

import (
	"context"
	"errors"
	"fmt"
	"log"
	"sync"
	"time"

	"github.com/holoplot/go-evdev"
	"github.com/nicholasyoder/niox/internal/config"
	"github.com/nicholasyoder/niox/internal/transport"
)

// dialRetryInterval is how long RunHost waits between failed Dial
// attempts -- a timeout just means the client isn't up yet.
const dialRetryInterval = 2 * time.Second

// HostConfig configures RunHost.
type HostConfig struct {
	Identity config.Identity
	// Peer is the one client this host drives (milestone 4 is
	// single-client; see ROADMAP.md milestone 5 for multi-client).
	Peer config.Peer

	// Hotkey codes are plain evdev key codes (e.g. 125 for KEY_LEFTMETA),
	// kept as uint16 here rather than evdev.EvCode so callers outside this
	// package (cmd/nioxd) don't need to import the evdev package
	// just to build a HostConfig. HotkeySecondary == 0 means "no
	// secondary key configured".
	HotkeyPrimary   uint16
	HotkeySecondary uint16

	// OnFocusChange, if set, is called (non-blocking, best-effort)
	// whenever the hotkey toggles focus.
	OnFocusChange func(Focus)
}

// RunHost captures this machine's keyboard/mouse, toggles focus between
// this machine and cfg.Peer on the configured hotkey, and streams events
// to whichever is focused -- locally through a Player (see ARCHITECTURE.md
// on why host-focused events still need local replay) or over an
// encrypted transport.Conn to the client. It blocks until ctx is done.
func RunHost(ctx context.Context, cfg HostConfig) error {
	devices, err := Discover()
	if err != nil {
		return fmt.Errorf("km: discover devices: %w", err)
	}
	if len(devices) == 0 {
		return fmt.Errorf("km: no keyboard/mouse devices found")
	}
	caps := Capabilities(devices)

	local, err := NewPlayer(caps)
	if err != nil {
		return fmt.Errorf("km: create local passthrough device: %w", err)
	}
	defer local.Close()

	conn, err := dialWithRetry(ctx, transport.Options{Identity: cfg.Identity, Peer: cfg.Peer})
	if err != nil {
		return err
	}
	defer conn.Close()

	capMsg, err := encodeCapabilities(caps)
	if err != nil {
		return err
	}
	if err := conn.Send(capMsg); err != nil {
		return fmt.Errorf("km: send capabilities: %w", err)
	}

	events := make(chan *evdev.InputEvent, 64)
	var wg sync.WaitGroup
	for _, dev := range devices {
		wg.Add(1)
		go func(dev *evdev.InputDevice) {
			defer wg.Done()
			if err := CaptureLoop(ctx, dev, events); err != nil {
				log.Printf("km: capture loop for %s exited: %v", dev.Path(), err)
			}
		}(dev)
	}

	switcher := NewSwitcher(evdev.EvCode(cfg.HotkeyPrimary), evdev.EvCode(cfg.HotkeySecondary))
	deliver := func(ev *evdev.InputEvent) error {
		if switcher.Focus() == FocusHost {
			return local.Write(ev)
		}
		return conn.Send(encodeEvent(ev))
	}

	for {
		select {
		case <-ctx.Done():
			wg.Wait()
			return nil

		case ev := <-events:
			forward, synthetic, changed := switcher.Process(ev)
			if synthetic != nil {
				if err := deliver(synthetic); err != nil {
					log.Printf("km: deliver synthetic event: %v", err)
				}
			}
			if forward {
				if err := deliver(ev); err != nil {
					log.Printf("km: deliver event: %v", err)
				}
			}
			if changed && cfg.OnFocusChange != nil {
				cfg.OnFocusChange(switcher.Focus())
			}
		}
	}
}

// dialWithRetry keeps attempting transport.Dial until it succeeds or ctx
// is done. A handshake timeout just means the client isn't listening yet;
// any other error is fatal.
func dialWithRetry(ctx context.Context, opts transport.Options) (*transport.Conn, error) {
	for {
		conn, err := transport.Dial(ctx, opts)
		if err == nil {
			return conn, nil
		}
		if !errors.Is(err, transport.ErrHandshakeTimeout) {
			return nil, fmt.Errorf("km: dial %s: %w", opts.Peer.Address, err)
		}
		log.Printf("km: waiting for client at %s...", opts.Peer.Address)
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(dialRetryInterval):
		}
	}
}
