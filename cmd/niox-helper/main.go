// Command niox-helper is the privileged process responsible for
// keyboard/mouse capture, virtual input, and the KM network hot path.
// See ARCHITECTURE.md for its role relative to niox-daemon.
package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"

	"github.com/nicholasyoder/niox/internal/config"
	"github.com/nicholasyoder/niox/internal/ipc"
	"github.com/nicholasyoder/niox/internal/km"
)

func defaultSocketPath() string {
	if dir := os.Getenv("XDG_RUNTIME_DIR"); dir != "" {
		return filepath.Join(dir, "niox", "niox.sock")
	}
	return filepath.Join(os.TempDir(), fmt.Sprintf("niox-%d.sock", os.Getuid()))
}

func main() {
	socketPath := flag.String("socket", defaultSocketPath(), "path to the niox-daemon control socket")
	flag.Parse()

	if err := run(*socketPath); err != nil {
		fmt.Fprintf(os.Stderr, "niox-helper: %v\n", err)
		os.Exit(1)
	}
}

// run connects to niox-daemon, fetches this machine's KM config over IPC
// (niox-helper never reads the TOML file itself -- see ARCHITECTURE.md),
// and runs the KM hot path as a host or client per the fetched role.
func run(socketPath string) error {
	conn, err := ipc.Dial(socketPath)
	if err != nil {
		return err
	}
	defer conn.Close()

	cfg, err := fetchConfig(conn)
	if err != nil {
		return err
	}
	if len(cfg.Peers) == 0 {
		return fmt.Errorf("niox-helper: no peers configured")
	}
	peer := cfg.Peers[0] // milestone 4 is single-client; see ROADMAP.md milestone 5.

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	switch cfg.Role {
	case "host":
		fmt.Printf("niox-helper: starting as host, driving %s (%s)\n", peer.Name, peer.Address)
		return km.RunHost(ctx, km.HostConfig{
			Identity:        cfg.Identity,
			Peer:            peer,
			HotkeyPrimary:   cfg.HotkeyPrimary,
			HotkeySecondary: cfg.HotkeySecondary,
			OnFocusChange:   func(f km.Focus) { reportFocus(conn, f, peer) },
		})
	case "client":
		fmt.Printf("niox-helper: starting as client, accepting from %s\n", peer.Name)
		return km.RunClient(ctx, km.ClientConfig{
			Identity: cfg.Identity,
			Peer:     peer,
		})
	default:
		return fmt.Errorf("niox-helper: unknown km role %q (want \"host\" or \"client\")", cfg.Role)
	}
}

// fetchConfig asks niox-daemon for this machine's KM config.
func fetchConfig(conn *ipc.Conn) (*ipc.KMConfig, error) {
	if err := conn.Send(ipc.Message{Type: ipc.MessageTypeHello}); err != nil {
		return nil, err
	}
	reply, err := conn.Recv()
	if err != nil {
		return nil, err
	}
	if reply.Type != ipc.MessageTypeConfig || reply.Config == nil {
		return nil, fmt.Errorf("niox-helper: unexpected reply to hello: %+v", reply)
	}
	return reply.Config, nil
}

// reportFocus sends a best-effort MessageTypeFocus update to niox-daemon.
func reportFocus(conn *ipc.Conn, f km.Focus, peer config.Peer) {
	target := peer.Name
	if f == km.FocusHost {
		target = "host"
	}
	if err := conn.Send(ipc.Message{Type: ipc.MessageTypeFocus, Focus: target}); err != nil {
		fmt.Fprintf(os.Stderr, "niox-helper: report focus: %v\n", err)
	}
}
