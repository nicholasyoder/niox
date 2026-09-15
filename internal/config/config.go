// Package config loads and validates niox's shared TOML configuration:
// machine identity/keypair, known peers, and per-subsystem settings. See
// ARCHITECTURE.md.
package config

import (
	"crypto/rand"
	"encoding/base64"
	"fmt"
	"os"

	"github.com/flynn/noise"
	"github.com/pelletier/go-toml/v2"
)

// Key is a fixed-size cryptographic key that (de)serializes as base64 text
// in the TOML config, matching the WireGuard-style key encoding rather than
// TOML's default integer-array representation for byte slices.
type Key []byte

func (k Key) MarshalText() ([]byte, error) {
	return []byte(base64.StdEncoding.EncodeToString(k)), nil
}

func (k *Key) UnmarshalText(text []byte) error {
	decoded, err := base64.StdEncoding.DecodeString(string(text))
	if err != nil {
		return fmt.Errorf("config: decode key: %w", err)
	}
	*k = decoded
	return nil
}

// Identity is this machine's long-lived Noise keypair. Peers trust this
// machine by listing PublicKey in their own config (see Peer) — there is no
// discovery or certificate authority.
type Identity struct {
	PrivateKey Key `toml:"private_key"`
	PublicKey  Key `toml:"public_key"`
}

// Peer is a known remote machine, trusted by listing its public key.
type Peer struct {
	Name      string `toml:"name"`
	Address   string `toml:"address"`
	PublicKey Key    `toml:"public_key"`
}

// Default hotkey codes (Linux evdev KEY_LEFTMETA and KEY_GRAVE), matching
// NKMS's own defaults so existing muscle memory carries over.
const (
	DefaultHotkeyPrimary   uint16 = 125
	DefaultHotkeySecondary uint16 = 41
)

// KM holds this machine's keyboard/mouse subsystem settings. Role selects
// whether this machine drives Peers (host) or is driven by them (client);
// milestone 4 (see ROADMAP.md) assumes exactly one peer either way --
// per-peer roles arrive with multi-client support.
type KM struct {
	Role            string `toml:"role"` // "host" or "client"
	HotkeyPrimary   uint16 `toml:"hotkey_primary,omitempty"`
	HotkeySecondary uint16 `toml:"hotkey_secondary,omitempty"` // 0 means no secondary key.
}

// Config is the full contents of one machine's config file.
type Config struct {
	Identity Identity `toml:"identity"`
	Peers    []Peer   `toml:"peers"`
	KM       KM       `toml:"km"`
}

// generateIdentity creates a fresh Noise DH25519 keypair for this machine.
func generateIdentity() (Identity, error) {
	key, err := noise.DH25519.GenerateKeypair(rand.Reader)
	if err != nil {
		return Identity{}, fmt.Errorf("config: generate identity keypair: %w", err)
	}
	return Identity{PrivateKey: Key(key.Private), PublicKey: Key(key.Public)}, nil
}

// Load reads the config file at path. If no file exists there, it generates
// a fresh identity keypair, writes a new config file to path, and returns
// it — so first run and steady-state load are the same code path.
func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		identity, err := generateIdentity()
		if err != nil {
			return nil, err
		}
		cfg := &Config{Identity: identity}
		cfg.applyKMDefaults()
		cfg.KM.HotkeySecondary = DefaultHotkeySecondary // only on first run -- see applyKMDefaults.
		if err := cfg.Save(path); err != nil {
			return nil, err
		}
		return cfg, nil
	}
	if err != nil {
		return nil, fmt.Errorf("config: read %s: %w", path, err)
	}

	var cfg Config
	if err := toml.Unmarshal(data, &cfg); err != nil {
		return nil, fmt.Errorf("config: parse %s: %w", path, err)
	}
	cfg.applyKMDefaults()
	return &cfg, nil
}

// applyKMDefaults fills in KM.HotkeyPrimary if unset (zero) -- a hotkey is
// always required, so zero can only mean "not yet configured," never a
// deliberate choice, unlike HotkeySecondary (see Load's first-run branch,
// where 0 legitimately means "no secondary key" once a config has been
// through a save cycle, and is left alone here on every later load).
func (cfg *Config) applyKMDefaults() {
	if cfg.KM.HotkeyPrimary == 0 {
		cfg.KM.HotkeyPrimary = DefaultHotkeyPrimary
	}
}

// Save serializes cfg as TOML and writes it to path, replacing any existing
// file. The file is written with 0600 permissions since it contains this
// machine's private key.
func (cfg *Config) Save(path string) error {
	data, err := toml.Marshal(cfg)
	if err != nil {
		return fmt.Errorf("config: marshal: %w", err)
	}
	if err := os.WriteFile(path, data, 0o600); err != nil {
		return fmt.Errorf("config: write %s: %w", path, err)
	}
	return nil
}
