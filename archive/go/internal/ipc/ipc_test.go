package ipc

import (
	"os"
	"path/filepath"
	"runtime"
	"testing"

	"github.com/nicholasyoder/niox/internal/config"
)

func TestListenDialPingStatus(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.sock")

	ln, err := Listen(path)
	if err != nil {
		t.Fatalf("Listen: %v", err)
	}
	defer ln.Close()

	serverErr := make(chan error, 1)
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			serverErr <- err
			return
		}
		defer conn.Close()

		msg, err := conn.Recv()
		if err != nil {
			serverErr <- err
			return
		}
		if msg.Type != MessageTypePing {
			serverErr <- err
			return
		}
		serverErr <- conn.Send(Message{Type: MessageTypeStatus, Ready: true})
	}()

	client, err := Dial(path)
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	defer client.Close()

	if err := client.Send(Message{Type: MessageTypePing}); err != nil {
		t.Fatalf("Send: %v", err)
	}

	reply, err := client.Recv()
	if err != nil {
		t.Fatalf("Recv: %v", err)
	}
	if reply.Type != MessageTypeStatus || !reply.Ready {
		t.Errorf("reply = %+v, want status with Ready=true", reply)
	}

	if err := <-serverErr; err != nil {
		t.Fatalf("server: %v", err)
	}
}

func TestListenDialHelloConfig(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.sock")

	ln, err := Listen(path)
	if err != nil {
		t.Fatalf("Listen: %v", err)
	}
	defer ln.Close()

	want := &KMConfig{
		Identity:      config.Identity{PrivateKey: config.Key{1}, PublicKey: config.Key{2}},
		Peers:         []config.Peer{{Name: "desk", Address: "192.168.1.20:4777", PublicKey: config.Key{9}}},
		Role:          "host",
		HotkeyPrimary: 125,
	}

	serverErr := make(chan error, 1)
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			serverErr <- err
			return
		}
		defer conn.Close()

		msg, err := conn.Recv()
		if err != nil {
			serverErr <- err
			return
		}
		if msg.Type != MessageTypeHello {
			serverErr <- err
			return
		}
		serverErr <- conn.Send(Message{Type: MessageTypeConfig, Config: want})
	}()

	client, err := Dial(path)
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	defer client.Close()

	if err := client.Send(Message{Type: MessageTypeHello}); err != nil {
		t.Fatalf("Send: %v", err)
	}

	reply, err := client.Recv()
	if err != nil {
		t.Fatalf("Recv: %v", err)
	}
	if reply.Type != MessageTypeConfig || reply.Config == nil {
		t.Fatalf("reply = %+v, want a config message", reply)
	}
	if reply.Config.Role != want.Role || reply.Config.HotkeyPrimary != want.HotkeyPrimary {
		t.Errorf("config = %+v, want %+v", reply.Config, want)
	}
	if len(reply.Config.Peers) != 1 || reply.Config.Peers[0].Name != "desk" {
		t.Errorf("peers = %+v, want one peer named desk", reply.Config.Peers)
	}

	if err := <-serverErr; err != nil {
		t.Fatalf("server: %v", err)
	}
}

func TestListenSetsSocketPermissions(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix permission bits don't apply on windows")
	}

	path := filepath.Join(t.TempDir(), "niox.sock")
	ln, err := Listen(path)
	if err != nil {
		t.Fatalf("Listen: %v", err)
	}
	defer ln.Close()

	info, err := os.Stat(path)
	if err != nil {
		t.Fatalf("stat: %v", err)
	}
	if mode := info.Mode().Perm(); mode != socketMode {
		t.Errorf("socket permissions = %o, want %o", mode, socketMode)
	}
}

func TestListenRemovesStaleSocketFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.sock")
	if err := os.WriteFile(path, []byte("stale"), 0o600); err != nil {
		t.Fatalf("write stale file: %v", err)
	}

	ln, err := Listen(path)
	if err != nil {
		t.Fatalf("Listen: %v", err)
	}
	ln.Close()
}

func TestDialNoListener(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.sock")
	if _, err := Dial(path); err == nil {
		t.Fatal("Dial: expected error, got nil")
	}
}
