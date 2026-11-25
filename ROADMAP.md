# niox Roadmap

Working order for building niox out, one verifiable step at a time. Items
are listed in the order to tackle them: the first unchecked item is
what's next. Steps are deliberately unnumbered so they can be added,
split, or reordered without renumbering anything; refer to a step by its
name. Each step should fit in roughly one working session; split any
that turn out bigger. See [ARCHITECTURE.md](ARCHITECTURE.md) for the
design each step implements.

**Current priority: keyboard/mouse at full Linux parity with NKMS, then
retire NKMS.** Audio comes after that, then the web UI. Fedora is the
primary target OS, with `.deb` packages kept as a secondary target.
macOS is deferred (see below).

## Ground rules

- niox is written in Rust as a single Cargo workspace (layout in
  ARCHITECTURE.md). Crates are created when a step first needs them.
- `archive/go/` holds an earlier Go prototype (also tagged
  `go-prototype`) that reached single-client KM. Use it as a reference
  for details that worked (packet framing, replay window, evdev device
  filtering, the hotkey state machine and its tests, NKMS-derived edge
  cases), not as something to port line by line. Several of its designs
  are superseded; see `archive/go/README.md`.
- Every step leaves the workspace passing `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo test`.
- Verification that needs real machines ("two machines", "3-machine
  test", login-screen checks) runs on the libvirt test VMs in
  `tools/testvm/` (host + two clients, GNOME and KDE), without waiting
  to be asked. Build on the workstation, copy the binaries over, and drive
  input with `virsh send-key` / `mouse_move`. Boot only the VMs a test
  needs, revert to `clean` when a run leaves them in a bad state, and shut
  them down afterward. Steps that need physical hardware still need the
  real machines; say so rather than counting a VM run as covering them.
- Design for the final shape up front where it's cheap: multiple peers,
  nioxd owning its config, and a transport that recovers on its own. No
  step should knowingly build something a later step has to tear out.

## Groundwork

Work that predates this roadmap. Completed steps below stay where they
are and just get checked off; nothing moves into this section.

- [x] **Go prototype** — config, a Noise KK transport spike, local IPC,
  and single-client KM over the encrypted transport, built in Go.
  Archived in `archive/go/` when niox moved to Rust, chosen for its audio and
  video ecosystems (PipeWire, GStreamer, real-time audio with no GC),
  the FFI Roc needs anyway, and consistency with the author's other
  projects.

## Keyboard/mouse: road to NKMS parity (Linux)

- [x] **Workspace scaffolding + config** — the Cargo workspace and the
  `niox-config` crate:
  - Workspace root, `crates/`, shared lints and release profile,
    `rust-toolchain.toml` pinned to the Fedora rustc, and `Cargo.lock`
    committed.
  - Decide and document in ARCHITECTURE.md: the concurrency model (the
    lean is plain threads + channels in nioxd, with no async runtime on
    the input hot path), the error-handling convention (`thiserror` in
    library crates, `anyhow` in binaries), and logging (`tracing`,
    to the journal under systemd).
  - Config schema and loading for `/etc/niox/niox.toml` via `serde` +
    `toml`: identity keypair (base64 X25519, generated on first run and
    written `0600`), peers with name, address, public key, and per-peer
    role, and KM settings (hotkeys, defaulting to NKMS's 125 + 41).
    Validation errors name the offending field. A `--config` override
    lets development run unprivileged against a local file.

  *Verification*: unit tests cover round-tripping, first-run key
  generation, file permissions, and rejection of malformed keys,
  duplicate peers, and unknown roles.

- [x] **Transport: handshake + packet layer** — the socket-free core of
  `niox-transport`:
  - Noise_KK_25519_ChaChaPoly_BLAKE2s via `snow`; data packets use
    `StatelessTransportState` with explicit nonces.
  - Datagram formats: versioned prefix, handshake init/response carrying
    sender indices and an init timestamp (rejects replayed inits), and a
    data header with receiver index + counter, within a 1280-byte max.
    The Go prototype's `packet.go` is a good starting point.
  - Sliding-window anti-replay.
  - A subsystem byte at the start of every data payload, so KM now and
    audio later share one session per peer.

  *Verification*: unit tests for every parse/encode path, the replay
  window's edges (duplicates, too-old, far-future, window slide), and a
  full in-memory handshake and data exchange. Tampered, truncated, and
  wrong-key datagrams are all rejected.

- [x] **Transport: peer sessions + liveness** — the socket half: one UDP
  socket per nioxd, serving every configured peer:
  - Demultiplex by receiver index and source address. Datagrams from
    unknown or unauthenticated sources are dropped silently.
  - Handshake retransmission with backoff, keepalives, and peer-liveness
    detection. Either side restarting, or a network drop, triggers an
    automatic re-handshake with no manual action. This replaces NKMS's
    "stay down 2s so clients notice" workaround.
  - Match an incoming init to its peer by source address, falling back to
    trying each configured peer's key (`respond` fails with `Auth` for the
    wrong one). Answer a retransmitted init by resending the cached
    response, since `respond` rejects it as a replay. Track each peer's
    newest accepted init timestamp, and resolve both sides initiating at
    once.
  - Per-peer up/down events for subsystems to react to.
  - Send/recv examples and a throughput/latency benchmark.

  *Verification*: two machines running the examples. Kill and restart
  each side, then both, and unplug the network for 30s; each case
  recovers within a few seconds. Record benchmark numbers in the commit.

- [x] **KM: protocol + focus logic** — the I/O-free part of `niox-km`:
  - Settle KM ordering (ARCHITECTURE.md open question). The lean is one
    datagram per evdev SYN frame so a frame can't be split or reordered
    internally, which may make a sequence number unnecessary. Record the
    decision in ARCHITECTURE.md.
  - Event wire encoding, and a reliable capabilities exchange (ack +
    resend) instead of a single unacknowledged datagram.
  - The hotkey state machine (port the Go prototype's tests, including
    the synthetic key1-press behavior), and focus cycling over N targets
    (host → client 1 → client 2 → … → host) with clients joining and
    leaving. Cover NKMS's client-removal edge cases (`old/nkms` commit
    31a289a) as unit tests: removing the focused client, the last
    client, or one before the focused client.
  - Held-key tracking, so everything held can be released on a target
    when focus leaves it or its connection drops.

  *Verification*: unit tests for all of the above, with no devices or
  sockets involved.

- [ ] **KM: local devices** — the device half of `niox-km`, via the
  `evdev` crate:
  - Keyboard/mouse device discovery and filtering, exclusive grab,
    capture loop, unioned capabilities, and uinput virtual device
    creation and playback.
  - Ungrab and virtual-device cleanup on every exit path, including
    panics and signals.

  *Verification*: on one machine, a test binary grabs the real keyboard
  and mouse and replays them through a uinput device, so input still
  works normally. After Ctrl-C and after a forced panic, nothing stays
  grabbed and `libinput list-devices` shows no leftover virtual device.

- [ ] **KM end-to-end, single client** — the `nioxd` binary as a
  standalone root service:
  - nioxd reads its own `/etc/niox/niox.toml`, joins the transport, and
    runs as host or client per config. It needs no user session.
  - The host sends to the focused client; the client plays into its
    virtual device. Held keys are released on focus change and on
    disconnect, on both sides.
  - A development systemd unit for testing at boot (final packaging
    comes later).

  *Verification*: on a client sitting at the GDM login screen, type the
  password from the host and log in. On a host with nobody logged in,
  drive a client. Restart host, client, then both mid-session, and
  unplug the network while holding a key; each recovers within a few
  seconds with nothing stuck down.

- [ ] **Multi-client + hot-plug** — N clients with focus cycling,
  clients joining and leaving at any time, and udev-monitor hot-plug of
  input devices:
  - When devices change, re-send the unioned capabilities to clients and
    recreate their virtual device only if capabilities grew (NKMS's
    `update_capabilities`). Release a grabbed device on any exit path.

  *Verification*: 3-machine test. Hot-plug a keyboard mid-session and
  confirm it works on a client without a restart. Remove the focused
  client and confirm focus moves sensibly.

- [ ] **Local control channel + nioxctl** — `niox-ipc` and the
  `nioxctl` CLI:
  - nioxd listens on `/run/niox/niox.sock` (root-owned, group `niox`,
    `0660`) with newline-delimited JSON, publishing focus and peer
    status to subscribers. It never carries per-event KM data.
  - `nioxctl status` (focus, connected peers), `nioxctl pubkey` (for
    pairing), and `nioxctl focus <target>`. This is the parity stand-in
    for NKMS's tray applet until the web UI exists.

  *Verification*: integration tests over a temp socket. On a real
  machine, a user in the `niox` group can run `nioxctl status`, and a
  user outside it is refused.

- [ ] **Live config + clean lifecycle** — config changes take effect
  without a manual restart (NKMS commit 463a341's behavior):
  - `systemctl reload nioxd` (SIGHUP) and `nioxctl reload` re-read
    config and apply hotkey and peer changes, re-handshaking only where
    needed.
  - Start/stop cycles leak nothing: no stray uinput devices, sockets, or
    grabs.

  *Verification*: change the hotkey, reload, and confirm the new hotkey
  works while clients stay connected. Run 20 reload and stop/start
  cycles and confirm `ls /dev/input/by-id` and `libinput list-devices`
  show no leftovers.

- [ ] **Packaging (RPM first, `.deb` second)** — one build producing
  both:
  - RPM (`rpm/niox.spec`, following NKMS's) as the primary package.
    Debian packaging is kept building alongside it.
  - A `nioxd` system unit (enabled at boot), the `niox` group, and
    `/etc/niox` and `/run/niox` setup.
  - Fedora specifics: a firewalld service definition for niox's UDP
    port, and confirm SELinux allows the system unit to access
    `/dev/uinput` and `/dev/input/*`.
  - Upgrades preserve the config and keypair.

  *Verification*: fresh Fedora install, install the RPM, pair with a
  peer, reboot, and confirm KM works from the login screen. Repeat the
  install on Debian with the `.deb`.

- [ ] ★ **Parity checkpoint: retire NKMS.** Replace NKMS with niox on
  every Linux machine and use it daily for a week. Anything NKMS did
  that niox doesn't becomes a bug against the steps above before moving
  on. Delete `archive/go/` once this passes.

## Audio

- [ ] **Audio transport spike** — prove that Roc can run inside niox's
  transport before building on it. Roc normally owns its own UDP
  sockets, so this means binding `libroc`'s packet-level encoder/decoder
  C API with `bindgen` (Roc has no official Rust bindings), against
  Fedora's packaged `roc-toolkit` 0.4, and carrying its packets in the
  transport's audio subsystem channel. If that isn't workable, pick and
  document the alternative explicitly (e.g. Opus over our own
  transport).
  *Verification*: test tone from one Fedora machine to another,
  encrypted.

- [ ] **niox-session skeleton** — the optional user-session binary:
  connects to nioxd's socket, subscribes to focus events, and gets the
  config slices it needs from nioxd. Includes its `systemctl --user`
  unit. Decide here how audio packets reach the network: through nioxd's
  transport session, or niox-session holding its own Noise session per
  peer (see ARCHITECTURE.md open questions).
  *Verification*: niox-session logs focus changes; killing and
  restarting it never affects KM.

- [ ] **Audio: one client → host** — capture from a dedicated PipeWire
  null sink set as the client's default (per
  `old/audio-switcher/CROSS_PLATFORM.md`, so it works with no speakers
  attached) via `pipewire-rs`, sent to the host and played to its output
  sink.
  *Verification*: real system audio from a client with no speakers
  plays on the host. Killing or restarting the client doesn't wedge the
  host.

- [ ] **Audio: multi-client mixing + focus-follow** — one receiver per
  client, a mixer with per-client gain (fixed at 1.0 or 0), and
  follow-focus vs. always-mix driven by focus events from nioxd.
  *Verification*: two clients mix cleanly. In follow-focus mode, cycling
  focus mutes and unmutes the right streams.

## Control API + web UI

- [ ] **Control API + web UI** (`niox-control`) — HTTP status (focus,
  peers, audio state) and control (switch focus, edit config via nioxd,
  mute, focus-follow toggle), LAN-reachable with token auth, usable from
  a phone. Must work on a headless host. Decide first where the HTTP
  server runs (see ARCHITECTURE.md open questions).
  *Verification*: switch focus, edit a peer, and toggle focus-follow
  from a phone browser, against a host with nobody logged in.

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
