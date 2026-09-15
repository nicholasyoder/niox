# niox Roadmap

Working order for building niox out, one verifiable milestone at a time.
See [ARCHITECTURE.md](ARCHITECTURE.md) for the design each milestone
implements. Milestones are ordered by dependency (each one builds on
what's already proven), not by final feature priority — KM reaches
usable parity with NKMS well before audio or packaging exist.

## Milestones

- [x] **0. Project scaffolding** — Go module, package skeleton
  (`cmd/niox-helper`, `cmd/niox-daemon`, empty `internal/*` packages),
  `ARCHITECTURE.md`, git init.

- [x] **1. Config foundation** (`internal/config`) — TOML load/save, a
  per-machine identity keypair (generated on first run), and a peer list
  (address + public key). No networking yet.
  *Verification*: round-trip tests reading/writing a config file; a
  `--print-config` smoke check confirms keypair generation is stable
  across restarts.

- [x] **2. Transport spike** (`internal/transport`) — Noise protocol
  handshake + per-packet ChaCha20-Poly1305 AEAD wrapping a UDP socket,
  proven standalone before anything depends on it — same spirit as
  audio-switcher's `spike-send`/`spike-recv` loopback test.
  *Verification*: two local processes complete a handshake and exchange
  encrypted messages over loopback; a peer with the wrong keypair is
  rejected; measure round-trip overhead to confirm it's negligible.

- [ ] **3. Local control channel** (`internal/ipc`) — Unix domain socket
  protocol between `niox-helper` and `niox-daemon`: connect, a trivial
  status/ping exchange, permission-restricted socket file.
  *Verification*: start both binaries by hand, confirm they connect and
  exchange a status message; confirm a non-owning user can't connect.

- [ ] **4. KM end-to-end, single client, Linux only** (`internal/km`,
  wired through milestones 2 and 3) — port NKMS's `evdev` capture,
  hotkey/focus detection, and `uinput` playback into Go, split across
  `niox-helper` (capture/send, receive/write) and `niox-daemon` (focus
  state, reported over IPC), with the KM stream running over the
  encrypted transport instead of NKMS's plaintext UDP.
  *Verification*: real hardware, host + one Linux client, hotkey toggles
  focus, latency feel matches NKMS today.

- [ ] **5. KM multi-client parity** — extend to N clients with NKMS's
  existing cycling order (host → client 1 → client 2 → … → host), plus
  its device hot-plug handling (`udev` monitor logic).
  *Verification*: 3-machine test; hot-plug a keyboard mid-session and
  confirm capabilities update without a restart.

- [ ] **6. Minimal control API + web UI** (`internal/control`) — HTTP
  server in `niox-daemon` exposing status (current focus, connected
  clients) and start/stop, LAN-reachable with token auth. No audio yet.
  *Verification*: switch focus and view status from a phone browser on
  the LAN.

- [ ] **7. Audio subsystem port** (`internal/audio`) — capture/playback
  via PipeWire/Pulse bindings, Roc transport, per-client receiver +
  mixer, all ported from audio-switcher, wrapped in the same
  Noise/AEAD transport as KM, and wired to the focus event bus so
  follow-focus (mute non-focused clients) vs. always-mix is a config
  toggle.
  *Verification*: mixing/muting behaves correctly as focus changes;
  matches audio-switcher's own milestone-3 mixing test.

- [ ] **8. Web UI: audio controls** — mute state, focus-follow toggle,
  per-client presence, all in the same UI as milestone 6.
  *Verification*: exercise the whole system (KM + audio) from the web UI
  on a headless Raspberry Pi host with no desktop session.

- [ ] **9. Packaging** — `.deb` for `niox-helper` (system unit, root) and
  `niox-daemon` (user unit), config file locations, install/upgrade
  scripts, mirroring both old projects' `debian/` setups.
  *Verification*: fresh install on a clean Debian machine, survives a
  reboot.

- [ ] **10. macOS support** — port `km`'s virtual input to macOS
  (cgo/Quartz bridge, akin to NKMS's existing
  `virtual_input_macos.py`), confirm Roc's Homebrew build works for
  audio capture/playback there.
  *Verification*: a mixed session — Linux host with a macOS client, or
  vice versa.

## Explicitly deferred

Tracked as future work, not scheduled into the milestones above:

- Video/graphics subsystem (needs its own design pass first — capture
  API, encode, latency budget).
- Windows support (Roc has no Windows support today).
- Mobile clients (Android/iOS capture APIs are a substantially different
  problem per `old/audio-switcher/CROSS_PLATFORM.md`).
- Tray icon convenience layer for desktop machines.
- Per-client audio volume beyond mute.
- Web UI auth beyond a static token (e.g. proper session login).
