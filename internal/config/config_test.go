package config

import (
	"bytes"
	"os"
	"path/filepath"
	"runtime"
	"testing"
)

func TestLoadGeneratesIdentityOnFirstRun(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.toml")

	cfg, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if len(cfg.Identity.PrivateKey) == 0 || len(cfg.Identity.PublicKey) == 0 {
		t.Fatalf("expected generated identity keys, got %+v", cfg.Identity)
	}
}

func TestLoadIsStableAcrossRestarts(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.toml")

	first, err := Load(path)
	if err != nil {
		t.Fatalf("first Load: %v", err)
	}

	second, err := Load(path)
	if err != nil {
		t.Fatalf("second Load: %v", err)
	}

	if !bytes.Equal(first.Identity.PrivateKey, second.Identity.PrivateKey) {
		t.Error("private key changed across restarts")
	}
	if !bytes.Equal(first.Identity.PublicKey, second.Identity.PublicKey) {
		t.Error("public key changed across restarts")
	}
}

func TestLoadAppliesKMDefaultsOnFirstRun(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.toml")

	cfg, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if cfg.KM.HotkeyPrimary != DefaultHotkeyPrimary {
		t.Errorf("HotkeyPrimary = %d, want default %d", cfg.KM.HotkeyPrimary, DefaultHotkeyPrimary)
	}
	if cfg.KM.HotkeySecondary != DefaultHotkeySecondary {
		t.Errorf("HotkeySecondary = %d, want default %d", cfg.KM.HotkeySecondary, DefaultHotkeySecondary)
	}

	// The defaults must have been persisted, not just applied in memory.
	reloaded, err := Load(path)
	if err != nil {
		t.Fatalf("reload: %v", err)
	}
	if reloaded.KM.HotkeyPrimary != DefaultHotkeyPrimary || reloaded.KM.HotkeySecondary != DefaultHotkeySecondary {
		t.Errorf("defaults did not survive a reload: %+v", reloaded.KM)
	}
}

func TestLoadPreservesExplicitSingleKeyHotkey(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.toml")

	// A user who wants single-key mode explicitly zeroes HotkeySecondary
	// and saves -- once persisted, that 0 must not be re-defaulted to 41
	// on the next load (only HotkeyPrimary=0 is always "unconfigured").
	cfg := &Config{
		Identity: Identity{PrivateKey: Key{1}, PublicKey: Key{2}},
		KM:       KM{Role: "host", HotkeyPrimary: 125, HotkeySecondary: 0},
	}
	if err := cfg.Save(path); err != nil {
		t.Fatalf("Save: %v", err)
	}

	got, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if got.KM.HotkeySecondary != 0 {
		t.Errorf("HotkeySecondary = %d, want 0 (explicit single-key mode)", got.KM.HotkeySecondary)
	}
}

func TestSaveLoadRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "niox.toml")

	want := &Config{
		Identity: Identity{
			PrivateKey: Key{1, 2, 3, 4},
			PublicKey:  Key{5, 6, 7, 8},
		},
		Peers: []Peer{
			{Name: "desk", Address: "192.168.1.20:4777", PublicKey: Key{9, 9, 9}},
			{Name: "laptop", Address: "192.168.1.21:4777", PublicKey: Key{8, 8, 8}},
		},
	}

	if err := want.Save(path); err != nil {
		t.Fatalf("Save: %v", err)
	}

	got, err := Load(path)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}

	if !bytes.Equal(got.Identity.PrivateKey, want.Identity.PrivateKey) {
		t.Errorf("private key = %v, want %v", got.Identity.PrivateKey, want.Identity.PrivateKey)
	}
	if !bytes.Equal(got.Identity.PublicKey, want.Identity.PublicKey) {
		t.Errorf("public key = %v, want %v", got.Identity.PublicKey, want.Identity.PublicKey)
	}
	if len(got.Peers) != len(want.Peers) {
		t.Fatalf("got %d peers, want %d", len(got.Peers), len(want.Peers))
	}
	for i := range want.Peers {
		if got.Peers[i].Name != want.Peers[i].Name {
			t.Errorf("peer[%d].Name = %q, want %q", i, got.Peers[i].Name, want.Peers[i].Name)
		}
		if got.Peers[i].Address != want.Peers[i].Address {
			t.Errorf("peer[%d].Address = %q, want %q", i, got.Peers[i].Address, want.Peers[i].Address)
		}
		if !bytes.Equal(got.Peers[i].PublicKey, want.Peers[i].PublicKey) {
			t.Errorf("peer[%d].PublicKey = %v, want %v", i, got.Peers[i].PublicKey, want.Peers[i].PublicKey)
		}
	}
}

func TestSaveWritesPrivatePermissions(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix permission bits don't apply on windows")
	}

	path := filepath.Join(t.TempDir(), "niox.toml")
	cfg := &Config{Identity: Identity{PrivateKey: Key{1}, PublicKey: Key{2}}}
	if err := cfg.Save(path); err != nil {
		t.Fatalf("Save: %v", err)
	}

	info, err := os.Stat(path)
	if err != nil {
		t.Fatalf("stat: %v", err)
	}
	if mode := info.Mode().Perm(); mode&0o077 != 0 {
		t.Errorf("config file permissions = %o, want no group/other bits", mode)
	}
}
