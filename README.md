# niox — Network I/O Exchange

niox lets one host machine (with a physical keyboard, mouse, and
eventually screen) control and exchange input, audio, and eventually
video with other machines over the LAN.

It's the successor to two earlier projects kept under `old/` for
reference: `nkms` (keyboard/mouse switching) and `audio-switcher`
(network audio mixing).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design and
[ROADMAP.md](ROADMAP.md) for progress.

**Status:** early development. Encrypted single-client keyboard/mouse
works on Linux. The current goal is full Linux parity with NKMS
(multi-client, hot-plug, login-screen support, RPM/deb packaging) so NKMS
can be retired. Audio and the web UI come after that.

## Layout

- `cmd/nioxd` -- privileged system service: config owner, KM capture/send/receive, virtual input
- `cmd/niox-session` -- optional user-session binary: audio, control API, web UI
- `internal/km`, `internal/audio`, `internal/video` -- per-subsystem logic
- `internal/transport` -- encrypted UDP transport shared by subsystems
- `internal/ipc` -- local control channel (nioxd listens; niox-session and nioxctl connect)
- `internal/control` -- HTTP API + web UI
- `internal/config` -- shared TOML config
- `web/` -- control UI frontend source
