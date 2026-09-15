// Command niox-daemon is the user-session process responsible for audio,
// the focus event bus, and the HTTP control API / web UI.
// See ARCHITECTURE.md for its role relative to niox-helper.
package main

import (
	"context"
	"encoding/base64"
	"flag"
	"fmt"
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
	socketPath := flag.String("socket", defaultSocketPath(), "path to the niox-helper control socket")
	printConfig := flag.Bool("print-config", false, "load (or create) the config file and print it, then exit")
	flag.Parse()

	if *printConfig {
		if err := os.MkdirAll(filepath.Dir(*configPath), 0o700); err != nil {
			fmt.Fprintf(os.Stderr, "niox-daemon: %v\n", err)
			os.Exit(1)
		}
		cfg, err := config.Load(*configPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "niox-daemon: %v\n", err)
			os.Exit(1)
		}
		fmt.Printf("config path:   %s\n", *configPath)
		fmt.Printf("public key:    %s\n", base64.StdEncoding.EncodeToString(cfg.Identity.PublicKey))
		fmt.Printf("known peers:   %d\n", len(cfg.Peers))
		return
	}

	if err := serve(*socketPath); err != nil {
		fmt.Fprintf(os.Stderr, "niox-daemon: %v\n", err)
		os.Exit(1)
	}
}

// serve listens on the control socket and answers each connection's ping
// with a status reply, until interrupted. The rest of niox-daemon (audio,
// focus bus, HTTP control API) isn't implemented yet -- this proves the
// control channel end to end per ROADMAP.md milestone 3.
func serve(socketPath string) error {
	ln, err := ipc.Listen(socketPath)
	if err != nil {
		return err
	}
	defer ln.Close()
	fmt.Printf("niox-daemon: listening on %s\n", socketPath)

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
		go handleConn(conn)
	}
}

func handleConn(conn *ipc.Conn) {
	defer conn.Close()

	msg, err := conn.Recv()
	if err != nil {
		fmt.Fprintf(os.Stderr, "niox-daemon: recv from %s: %v\n", conn.RemoteAddr(), err)
		return
	}
	if msg.Type != ipc.MessageTypePing {
		fmt.Fprintf(os.Stderr, "niox-daemon: unexpected message type %q\n", msg.Type)
		return
	}

	if err := conn.Send(ipc.Message{Type: ipc.MessageTypeStatus, Ready: true}); err != nil {
		fmt.Fprintf(os.Stderr, "niox-daemon: send to %s: %v\n", conn.RemoteAddr(), err)
	}
}
