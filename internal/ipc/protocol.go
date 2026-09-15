package ipc

import "github.com/nicholasyoder/niox/internal/config"

// Message is one control message exchanged over the channel, encoded as a
// single line of JSON (see doc.go). Type selects which of the other fields
// are meaningful; niox-helper and niox-daemon agree on the vocabulary out
// of band rather than through a polymorphic payload, since the channel
// only carries a handful of low-frequency message kinds.
type Message struct {
	Type    string `json:"type"`
	Ready   bool   `json:"ready,omitempty"`
	Version string `json:"version,omitempty"`

	// Config carries niox-daemon's reply to MessageTypeHello.
	Config *KMConfig `json:"config,omitempty"`
	// Focus carries niox-helper's MessageTypeFocus updates: "host" or the
	// name of the peer currently in focus.
	Focus string `json:"focus,omitempty"`
}

const (
	// MessageTypePing is sent by niox-helper to check that niox-daemon is
	// alive and reachable over the channel.
	MessageTypePing = "ping"
	// MessageTypeStatus is niox-daemon's reply to MessageTypePing.
	MessageTypeStatus = "status"

	// MessageTypeHello is sent by niox-helper on startup to request the
	// KM-relevant slice of config it needs to run its hot path.
	// niox-helper never reads the TOML config file itself -- see
	// ARCHITECTURE.md: niox-daemon owns config and pushes it down over
	// this channel.
	MessageTypeHello = "hello"
	// MessageTypeConfig is niox-daemon's reply to MessageTypeHello.
	MessageTypeConfig = "config"
	// MessageTypeFocus is sent by niox-helper whenever the KM hotkey
	// toggles focus, so niox-daemon can report current status (e.g. to a
	// future web UI). It never carries per-event KM data -- see doc.go.
	MessageTypeFocus = "focus"
)

// KMConfig is the KM-relevant slice of a machine's config, sent from
// niox-daemon to niox-helper in a MessageTypeConfig reply.
type KMConfig struct {
	Identity        config.Identity `json:"identity"`
	Peers           []config.Peer   `json:"peers"`
	Role            string          `json:"role"`
	HotkeyPrimary   uint16          `json:"hotkey_primary"`
	HotkeySecondary uint16          `json:"hotkey_secondary"`
}
