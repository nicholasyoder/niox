// Command niox-session is the user-session process responsible for audio,
// the focus event bus, and the HTTP control API / web UI.
// See ARCHITECTURE.md for its role relative to nioxd.
package main

import (
	"context"
	"encoding/base64"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"

	"github.com/nicholasyoder/niox/internal/config"
	"github.com/nicholasyoder/niox/internal/ipc"
)

func defaultConfigPath() string {
	if dir, err := os.UserConfigDir(); err == nil {
		return filepath.Join(dir, "niox", "niox.toml")
	}
	return "niox.toml"
}

func defaultSocketPath() string {
	if dir := os.Getenv("XDG_RUNTIME_DIR"); dir != "" {
		return filepath.Join(dir, "niox", "niox.sock")
	}
	return filepath.Join(os.TempDir(), fmt.Sprintf("niox-%d.sock", os.Getuid()))
}

func main() {
	configPath := flag.String("config", defaultConfigPath(), "path to niox config file")
	socketPath := flag.String("socket", defaultSocketPath(), "path to the nioxd control socket")
	printConfig := flag.Bool("print-config", false, "load (or create) the config file and print it, then exit")
	flag.Parse()

	if err := os.MkdirAll(filepath.Dir(*configPath), 0o700); err != nil {
		fmt.Fprintf(os.Stderr, "niox-session: %v\n", err)
		os.Exit(1)
	}
	cfg, err := config.Load(*configPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "niox-session: %v\n", err)
		os.Exit(1)
	}

	if *printConfig {
		fmt.Printf("config path:   %s\n", *configPath)
		fmt.Printf("public key:    %s\n", base64.StdEncoding.EncodeToString(cfg.Identity.PublicKey))
		fmt.Printf("known peers:   %d\n", len(cfg.Peers))
		fmt.Printf("km role:       %s\n", cfg.KM.Role)
		return
	}

	if err := serve(*socketPath, cfg); err != nil {
		fmt.Fprintf(os.Stderr, "niox-session: %v\n", err)
		os.Exit(1)
	}
}

// serve listens on the control socket and, per connection, answers pings,
// answers MessageTypeHello with the KM config nioxd needs to run its
// hot path (see ARCHITECTURE.md -- nioxd never reads the TOML file
// itself), and logs MessageTypeFocus updates, until interrupted.
func serve(socketPath string, cfg *config.Config) error {
	ln, err := ipc.Listen(socketPath)
	if err != nil {
		return err
	}
	defer ln.Close()
	fmt.Printf("niox-session: listening on %s\n", socketPath)

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go func() {
		<-ctx.Done()
		ln.Close()
	}()

	for {
		conn, err := ln.Accept()
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			return err
		}
		go handleConn(conn, cfg)
	}
}

func handleConn(conn *ipc.Conn, cfg *config.Config) {
	defer conn.Close()

	for {
		msg, err := conn.Recv()
		if err != nil {
			if !errors.Is(err, io.EOF) {
				fmt.Fprintf(os.Stderr, "niox-session: recv from %s: %v\n", conn.RemoteAddr(), err)
			}
			return
		}

		var reply ipc.Message
		switch msg.Type {
		case ipc.MessageTypePing:
			reply = ipc.Message{Type: ipc.MessageTypeStatus, Ready: true}
		case ipc.MessageTypeHello:
			reply = ipc.Message{Type: ipc.MessageTypeConfig, Config: &ipc.KMConfig{
				Identity:        cfg.Identity,
				Peers:           cfg.Peers,
				Role:            cfg.KM.Role,
				HotkeyPrimary:   cfg.KM.HotkeyPrimary,
				HotkeySecondary: cfg.KM.HotkeySecondary,
			}}
		case ipc.MessageTypeFocus:
			fmt.Printf("niox-session: focus changed to %q\n", msg.Focus)
			continue
		default:
			fmt.Fprintf(os.Stderr, "niox-session: unexpected message type %q\n", msg.Type)
			return
		}

		if err := conn.Send(reply); err != nil {
			fmt.Fprintf(os.Stderr, "niox-session: send to %s: %v\n", conn.RemoteAddr(), err)
			return
		}
	}
}
