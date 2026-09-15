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

// Config is the full contents of one machine's config file.
type Config struct {
	Identity Identity `toml:"identity"`
	Peers    []Peer   `toml:"peers"`
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
	return &cfg, nil
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
