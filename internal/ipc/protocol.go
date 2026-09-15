package ipc

// Message is one control message exchanged over the channel, encoded as a
// single line of JSON (see doc.go). Type selects which of the other fields
// are meaningful; niox-helper and niox-daemon agree on the vocabulary out
// of band rather than through a polymorphic payload, since the channel
// only carries a handful of low-frequency message kinds.
type Message struct {
	Type    string `json:"type"`
	Ready   bool   `json:"ready,omitempty"`
	Version string `json:"version,omitempty"`
}

const (
	// MessageTypePing is sent by niox-helper to check that niox-daemon is
	// alive and reachable over the channel.
	MessageTypePing = "ping"
	// MessageTypeStatus is niox-daemon's reply to MessageTypePing.
	MessageTypeStatus = "status"
)
