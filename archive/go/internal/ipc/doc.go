// Package ipc implements the local Unix-domain-socket control channel
// between nioxd and niox-session. It carries only low-frequency
// control messages (focus changes, config reloads, status) -- never
// per-event KM data. See ARCHITECTURE.md.
package ipc
