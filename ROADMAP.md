# niox Roadmap

Working order for building niox out, one verifiable step at a time. Items
are listed in the order to tackle them: the first unchecked item is
what's next. Steps are deliberately unnumbered so they can be added,
split, or reordered without renumbering everything after them; refer to
a step by its name. See [ARCHITECTURE.md](ARCHITECTURE.md) for the
design each step implements.

**Current priority: port the existing Go code to Rust, then reach full
Linux parity with NKMS and retire NKMS.** Everything up to the parity
checkpoint is keyboard/mouse only. Audio, the web UI, and other new
capabilities come after it. Fedora is the primary target OS, with `.deb`
packages kept as a secondary target. macOS is deferred (see below).

## Done (Go implementation)

Built in Go before the decision to move to Rust. This code is the
reference spec for the port below and is deleted at the cutover step.

- [x] **Project scaffolding** — Go module, package skeleton,
  `ARCHITECTURE.md`, git init.

- [x] **Config foundation** (`internal/config`) — TOML load/save, a
  per-machine identity keypair (generated on first run), and a peer list
  (address + public key).

- [x] **Transport spike** (`internal/transport`) — Noise KK handshake
  + per-packet ChaCha20-Poly1305 AEAD over UDP, with anti-replay.
  Deliberately spike-scoped: one handshake attempt, no keepalive, no
  re-handshake (picked up in KM hardening).

- [x] **Local control channel** (`internal/ipc`) — Unix domain socket,
  newline-delimited JSON, permission-restricted socket file.

- [x] **KM end-to-end, single client** (`internal/km`) — evdev
  capture, hotkey/focus switching, and uinput playback ported from NKMS,
  running over the encrypted transport. At this stage nioxd takes its
  config from niox-session over IPC, which nioxd independence reverses.

## Port to Rust

Why Rust: audio (PipeWire, real-time callbacks with no GC) and later
video (GStreamer) are much better served by Rust's ecosystem, Roc's C
library needs FFI either way (which erases Go's static-binary advantage),
and Rust matches the rest of the author's projects. The Go code is small
enough (~2.9k lines plus ~1.3k lines of tests) that porting now is cheap.

Ground rules for every port step:

- **Behavior parity, not redesign.** Port what the Go code does,
  including its wire formats, so Go and Rust builds can talk to each
  other during the port. Redesigns already scheduled (IPC direction,
  keepalives, multi-client) stay in their own parity steps. Fix obvious
  bugs, but note each one in the commit.
- **Tests come along.** Port each Go `_test.go` file alongside its code.
  The Go tests are the acceptance criteria.
- **Go and Rust coexist** until the cutover step. Rust lives in a Cargo
  workspace at the repo root (layout in ARCHITECTURE.md); Go stays in
  `cmd/` and `internal/` untouched.
- **Interop is the verification.** Where a step has a wire or file
  format, check it against the Go build: Rust ↔ Go, both directions.
- `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` pass at
  the end of every step.

- [ ] **Workspace scaffolding + config** — Cargo workspace, crate
  skeleton, toolchain pin, and the `niox-config` crate:
  - Workspace with `crates/` per ARCHITECTURE.md, shared lint and
    profile settings, `rust-toolchain.toml` pinned to the Fedora rustc
    version, and `Cargo.lock` committed.
  - Decide and document the concurrency model in ARCHITECTURE.md. The
    lean: plain threads + channels in `nioxd` (a handful of
    long-lived loops, no async runtime on the input hot path); async only
    if `niox-session`'s HTTP server needs it later.
  - Pick the error-handling convention (`thiserror` in library crates,
    `anyhow` in binaries).
  - Port `internal/config`: TOML load/save via `serde` + `toml`, base64
    keys, first-run X25519 keypair generation, and the KM defaults
    (hotkeys 125 + 41).

  *Verification*: config tests ported and passing. The Rust crate loads
  a `niox.toml` written by the Go build, and Go loads one written by
  Rust, with identical keys, peers, and KM settings.

- [ ] **Transport: handshake + packet layer** — the pure, socket-free
  half of `internal/transport` in `niox-transport`:
  - Noise_KK_25519_ChaChaPoly_BLAKE2s via `snow` (handshake state, then
    `StatelessTransportState` for explicit-nonce data packets).
  - Port the datagram formats in `packet.go` byte-for-byte (prefix,
    init/response payloads, data header with receiver index + counter,
    1280-byte max datagram) and `session.go`'s seal/open.
  - Port the anti-replay window (`replay.go`).

  *Verification*: replay-window and packet tests ported and passing. A
  test fixture of handshake + data datagrams captured from the Go
  implementation opens correctly in Rust, given the same keys.

- [ ] **Transport: connections + interop** — the socket half:
  - `Conn` with dial/accept, handshake timeout, send/recv, read
    deadline, stats, and close, matching `conn.go` semantics (silent
    drops of stale, duplicate, or unauthenticated datagrams).
  - Port the spike send/recv tools as `niox-transport` examples, and the
    benchmarks (`criterion`, or a `cargo bench` harness).
  - Port `transport_test.go`.

  *Verification*: tests passing. Rust sender → Go receiver and Go sender
  → Rust receiver both complete a handshake and pass data across two
  machines. Benchmark numbers recorded in the commit and roughly in line
  with Go's.

- [ ] **IPC** — port `internal/ipc` to `niox-ipc`: Unix domain socket,
  newline-delimited JSON via `serde_json`, the same message vocabulary,
  and the permission-restricted socket file.

  *Verification*: `ipc_test.go` ported and passing. A Rust client talks
  to the Go `niox-session` (hello → config, ping → status) and a Go
  client talks to a Rust listener.

- [ ] **KM: wire format + hotkey logic** — the pure half of
  `internal/km` in `niox-km`: the 9-byte event encoding, JSON
  capabilities messages, and the hotkey `Switcher` state machine
  (including the synthetic key1-press behavior).

  *Verification*: `wire_test.go` and `hotkey_test.go` ported and
  passing. Encoded events and capabilities are byte-identical to Go's
  for the same inputs.

- [ ] **KM: devices, capture, playback** — the I/O half of `niox-km`
  using the `evdev` crate (including its uinput support): device
  discovery and filtering (`devices.go`), grab + capture loop, unioned
  capabilities, virtual device creation and playback, and the
  `run_host` / `run_client` loops with connect retry.

  *Verification*: on one machine, a Rust test binary grabs the local
  keyboard and mouse and replays them through a uinput device. Ungrab
  and virtual-device cleanup happen on every exit path, including
  Ctrl-C.

- [ ] **Binaries + end-to-end** — `nioxd` and `niox-session` binaries
  matching today's Go flags and behavior (socket path, config path,
  `--print-config`, hello/config handshake, focus reports), with
  `clap` for argument parsing.

  *Verification*: repeat the Go build's KM end-to-end test entirely
  on Rust binaries: host drives one client over the encrypted transport,
  the hotkey switches focus, and niox-session logs focus changes. Then
  mix: Rust host with Go client, and Go host with Rust client.

- [ ] **Cutover** — delete the Go code (`cmd/`, `internal/`, `go.mod`,
  `go.sum`), update `.gitignore` (`/target`), README's layout section,
  and ARCHITECTURE.md (remove port-in-progress notes and any remaining Go
  references). Move this section's items into "Done".

  *Verification*: a fresh clone builds and tests with `cargo build` and
  `cargo test` alone. `grep -ri golang` finds nothing outside `old/`.

## Road to NKMS parity (Linux)

- [ ] **nioxd independence** — make `nioxd` a standalone
  system service that works with no user logged in (login screen,
  headless host), per ARCHITECTURE.md's privilege model:
  - nioxd reads its own root-owned config
    (`/etc/niox/niox.toml`, `0600`), which holds the identity keypair,
    peers, and KM settings. It no longer depends on niox-session to start.
  - The IPC direction flips: nioxd listens on
    `/run/niox/niox.sock` (root-owned, group `niox`, `0660`) and
    niox-session connects to it as an optional client. Session-side
    requests that change config (from the future web UI) go through the
    nioxd, which owns the file.
  - KM keeps working if niox-session is absent, crashes, or restarts.

  *Verification*: on a client sitting at the GDM login screen, type the
  password from the host and log in. On a host with nobody logged in,
  drive a client. Kill and restart niox-session and confirm KM is
  unaffected.

- [ ] **KM hardening** — the robustness NKMS earned the hard way, done
  properly at the transport/protocol level:
  - Keepalives and peer-liveness detection in `transport`. Either side
    restarting, or a network drop, triggers an automatic re-handshake
    with no manual action. This replaces NKMS's "stay down 2s so clients
    notice" workaround.
  - Reliable capabilities exchange (ack + resend). Today it is a single
    unacknowledged UDP datagram.
  - No stuck keys or buttons: release everything held on the outgoing
    target when focus switches or the connection drops, and release
    everything on the client's virtual device when it loses the host.
  - Decide how KM handles reordering. Either add a sequence number to
    the event wire format, or batch each evdev SYN frame into one
    datagram so a frame can't be split or reordered internally.

  *Verification*: restart host, then client, then both, mid-session. Each
  recovers within a few seconds with nothing stuck down. Unplug the
  network cable while holding a key, then reconnect. Drop the first
  capabilities packet (a test hook) and confirm the client still comes up.

- [ ] **Multi-client + hot-plug** — N clients with NKMS's cycling
  order (host → client 1 → client 2 → … → host), clients joining and
  leaving at any time, and udev-monitor hot-plug of input devices:
  - Port NKMS's client-removal index handling (`old/nkms` commit
    31a289a) and its edge cases as unit tests: removing the focused
    client, the last client, or a client before the focused one.
  - When devices change, re-send the unioned capabilities to clients and
    recreate their virtual device only if capabilities grew (NKMS's
    `update_capabilities`). Release a grabbed device on any exit path.
  - Per-peer roles in config, replacing the single `km.role`.

  *Verification*: 3-machine test. Hot-plug a keyboard mid-session and
  confirm it works on a client without a restart. Remove the focused
  client and confirm focus moves sensibly.

- [ ] **Live config + clean lifecycle** — config changes take effect
  without a manual restart (NKMS commit 463a341's behavior):
  - `systemctl reload nioxd` (SIGHUP) re-reads config and applies
    hotkey and peer changes, re-handshaking only where needed.
  - Start/stop cycles leak nothing: no stray uinput devices, sockets, or
    grabs.
  - A minimal `nioxctl` CLI talking to the nioxd socket: status
    (focus, connected peers), reload, and print-pubkey for pairing.
    This is the parity stand-in for NKMS's tray applet until the web UI
    exists.

  *Verification*: change the hotkey, reload, and confirm the new hotkey
  works while clients stay connected. Run 20 reload and stop/start
  cycles and confirm `ls /dev/input/by-id` and `libinput list-devices`
  show no leftovers.

- [ ] **Packaging (RPM first, `.deb` second)** — one build producing
  both:
  - RPM (`rpm/niox.spec`, following NKMS's) as the primary package.
    Debian packaging is kept building alongside it.
  - A `nioxd` system unit (enabled at boot) and a `niox-session`
    user unit, plus the `niox` group and `/etc/niox` and `/run/niox`
    setup.
  - Fedora specifics: a firewalld service definition for niox's UDP
    port(s), and confirm SELinux allows the system unit to access
    `/dev/uinput` and `/dev/input/*`.
  - Upgrades preserve the config and keypair.

  *Verification*: fresh Fedora install, install the RPM, pair with a
  peer, reboot, and confirm KM works from the login screen. Repeat the
  install on Debian with the `.deb`.

- [ ] ★ **Parity checkpoint: retire NKMS.** Replace NKMS with niox on
  every Linux machine and use it daily for a week. Anything NKMS did
  that niox doesn't becomes a bug against the parity items above
  before moving on.

## After parity

- [ ] **Control API + web UI** (`niox-control`) — HTTP status
  (focus, peers) and control (switch focus, start/stop, edit
  config via nioxd), LAN-reachable with token auth, usable from a
  phone. Must work on a headless host. Decide first where the HTTP
  server runs (see ARCHITECTURE.md open questions).
  *Verification*: switch focus and edit a peer from a phone browser,
  against a host with nobody logged in.

- [ ] **Audio transport spike** — prove that Roc can run inside the
  Noise transport before building on it. Roc normally owns its own UDP
  sockets, so this means using Roc's packet-level encoder/decoder API
  (binding `libroc`'s C API with `bindgen`, since Roc has no official
  Rust bindings) and carrying its packets over the transport's `Conn`. If that isn't workable,
  pick and document the alternative explicitly (e.g. Opus over our own
  transport).
  *Verification*: test tone from one Fedora machine to another,
  encrypted, with Fedora's packaged `roc-toolkit` 0.4.

- [ ] **Audio: one client → host** — capture from a dedicated
  PipeWire null sink set as the client's default (per
  `old/audio-switcher/CROSS_PLATFORM.md`, so it works with no speakers
  attached), sent to the host and played to its output sink. This is
  mostly new work: audio-switcher only reached its milestone 1.
  *Verification*: real system audio from a client with no speakers
  plays on the host. Killing or restarting the client doesn't wedge the
  host.

- [ ] **Audio: multi-client mixing + focus-follow** — one receiver
  per client, a mixer with per-client gain (fixed at 1.0 or 0), and
  follow-focus vs. always-mix driven by focus events from nioxd.
  *Verification*: two clients mix cleanly. In follow-focus mode, cycling
  focus mutes and unmutes the right streams.

- [ ] **Web UI: audio controls** — mute state, focus-follow toggle,
  and per-client presence in the control web UI.
  *Verification*: exercise KM + audio entirely from the web UI on a
  headless host.

## Explicitly deferred

Tracked as future work, not scheduled into the items above:

- **macOS support.** Not a current focus. If it returns, NKMS already has
  a working macOS client to port (`old/nkms/nkms/core/virtual_input_macos.py`:
  Quartz event posting, drag and double-click handling, and
  multi-display edge clamping, plus a LaunchAgent and menu-bar applet).
  macOS doesn't fit the Linux root-service/user-session split. It would be
  a single user-level process with the Accessibility permission.
- Video/graphics subsystem (needs its own design pass first — capture
  API, encode, latency budget).
- Clipboard sharing between host and clients.
- Windows support (Roc has no Windows support today).
- Mobile clients.
- Tray icon convenience layer for desktop machines.
- Per-client audio volume beyond mute.
- Web UI auth beyond a static token (e.g. proper session login).
