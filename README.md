# niox — Network I/O Exchange

niox lets one host machine (with a physical keyboard, mouse, and
eventually screen) control and exchange input, audio, and eventually
video with other machines over the LAN.

It's the successor to two earlier projects kept under `old/` for
reference: `nkms` (keyboard/mouse switching) and `audio-switcher`
(network audio mixing).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design. Nothing here
is implemented yet -- this is initial project scaffolding.

## Layout

- `cmd/niox-helper` -- privileged binary: KM capture/send/receive, virtual input
- `cmd/niox-daemon` -- user-session binary: audio, control API, web UI
- `internal/km`, `internal/audio`, `internal/video` -- per-subsystem logic
- `internal/transport` -- encrypted UDP transport shared by subsystems
- `internal/ipc` -- local control channel between the two binaries
- `internal/control` -- HTTP API + web UI
- `internal/config` -- shared TOML config
- `web/` -- control UI frontend source
