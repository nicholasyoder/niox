# Go prototype (archived)

niox's original Go implementation, built through single-client
keyboard/mouse (config, Noise KK transport spike, local IPC, evdev
capture/uinput playback). niox is now being built from scratch in Rust;
this code is kept only as a reference for that work and isn't built or
maintained. It can be deleted once the parity checkpoint in ROADMAP.md is
reached. The `go-prototype` git tag marks the last commit with it at the
repo root.

Note that it predates several designs in ARCHITECTURE.md: here
niox-session owns config and pushes it to nioxd over IPC, there is
exactly one peer, and the transport has no keepalive or re-handshake.
Don't port those parts as-is.
