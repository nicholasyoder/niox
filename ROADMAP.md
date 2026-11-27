# niox Roadmap

Working order for building niox out, one verifiable step at a time. Items
are listed in the order to tackle them: the first unchecked item is
what's next. Steps are deliberately unnumbered so they can be added,
split, or reordered without renumbering anything; refer to a step by its
name. Each step should fit in roughly one working session; split any
that turn out bigger. See [ARCHITECTURE.md](ARCHITECTURE.md) for the
design each step implements.

**Current priority: keyboard/mouse at full Linux parity with NKMS, then
retire NKMS.** Audio comes after that, then the tray icon and settings
window. Fedora is the primary target OS, with `.deb` packages kept as a
secondary target. macOS is deferred (see below).

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
  `tools/testvm/` (host + two clients, GNOME and KDE, plus a Debian VM
  for `.deb` checks), without waiting
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

- [x] **KM: local devices** — the device half of `niox-km`, via the
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

- [x] **KM end-to-end, single client** — the `nioxd` binary as a
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

- [x] **Multi-client + hot-plug** — N clients with focus cycling,
  clients joining and leaving at any time, and udev-monitor hot-plug of
  input devices:
  - When devices change, re-send the unioned capabilities to clients and
    recreate their virtual device only if capabilities grew (NKMS's
    `update_capabilities`). Release a grabbed device on any exit path.

  *Verification*: 3-machine test. Hot-plug a keyboard mid-session and
  confirm it works on a client without a restart. Remove the focused
  client and confirm focus moves sensibly.

- [x] **Local control channel + nioxctl** — `niox-ipc` and the
  `nioxctl` CLI:
  - nioxd listens on `/run/niox/niox.sock` (root-owned, group `niox`,
    `0660`) with newline-delimited JSON, publishing focus and peer
    status to subscribers. It never carries per-event KM data.
  - `nioxctl status` (focus, connected peers), `nioxctl pubkey` (for
    pairing), and `nioxctl focus <target>`. This is the parity stand-in
    for NKMS's tray applet until the tray icon exists.

  *Verification*: integration tests over a temp socket. On a real
  machine, a user in the `niox` group can run `nioxctl status`, and a
  user outside it is refused.

- [x] **Live config + clean lifecycle** — config changes take effect
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

- [x] **Packaging (RPM first, `.deb` second)** — one build producing
  both:
  - RPM (`rpm/niox.spec`, following NKMS's) as the primary package.
    Debian packaging is kept building alongside it.
  - A `nioxd` system unit (enabled at boot), the `niox` group, and
    `/etc/niox` and `/run/niox` setup.
  - Fedora specifics: a firewalld service definition for niox's UDP
    port, and confirm SELinux allows the system unit to access
    `/dev/uinput` and `/dev/input/*`.
  - If the unit is sandboxed, keep what KM needs: `AF_NETLINK` (hot-plug
    listens to kernel uevents) alongside `AF_INET`/`AF_INET6` and
    `AF_UNIX`, and no `PrivateNetwork=`, which hides uevents.
  - Upgrades preserve the config and keypair.

  *Verification*: fresh Fedora install, install the RPM, pair with a
  peer, reboot, and confirm KM works from the login screen. Repeat the
  install on Debian with the `.deb`.

- [ ] ★ **Parity checkpoint: retire NKMS.** Replace NKMS with niox on
  every Linux machine and use it daily for a week. Anything NKMS did
  that niox doesn't becomes a bug against the steps above before moving
  on. Delete `archive/go/` once this passes.

## Audio

- [x] **Audio transport spike** — prove that Roc can run inside niox's
  transport before building on it. Roc normally owns its own UDP
  sockets, so this means binding `libroc`'s packet-level encoder/decoder
  C API with `bindgen` (Roc has no official Rust bindings), against
  Fedora's packaged `roc-toolkit` 0.4, and carrying its packets in the
  transport's audio subsystem channel. If that isn't workable, pick and
  document the alternative explicitly (e.g. Opus over our own
  transport).
  *Verification*: test tone from one Fedora machine to another,
  encrypted.

- [x] **niox-session skeleton** — the optional user-session binary:
  connects to nioxd's socket, subscribes to focus events, and gets the
  config slices it needs from nioxd. Includes its `systemctl --user`
  unit. Decide here how audio packets reach the network: through nioxd's
  transport session, or niox-session holding its own Noise session per
  peer (see ARCHITECTURE.md open questions). Also decide which
  niox-session nioxd serves when more than one user is logged in (fast
  user switching, or a lingering user plus a desktop login).
  Decided: audio goes through nioxd's session, and the active seat's
  user gets it (see ARCHITECTURE.md); building that is the next step.
  *Verification*: niox-session logs focus changes; killing and
  restarting it never affects KM.

- [x] **Audio channel through nioxd** — the path audio takes to the
  network, as decided in the niox-session skeleton step:
  - An `audio` request on the control socket, answered with one end of
    a `SOCK_SEQPACKET` socketpair passed over `SCM_RIGHTS`, living as
    long as that control connection.
  - nioxd forwards Roc packets both ways as `Subsystem::Audio`: outbound
    sealed and sent per peer, inbound written from the transport's
    driver thread without blocking, dropped when niox-session lags.
  - Active-seat arbitration: nioxd grants the channel to the
    connection whose `SO_PEERCRED` uid is `seat0`'s `ACTIVE_UID`, moves
    it when the active user changes, and falls back to first come when
    no seat has an active user.
  *Verification*: the audio-tone example, run through niox-session's
  channel instead of its own transport, carries a tone between two VMs.
  Switching the active user moves the channel. A niox-session that stops
  reading never delays KM, and `kill -9` of it frees the channel.

- [x] **Audio: one client → host** — capture from a dedicated PipeWire
  null sink set as the client's default (per
  `old/audio-switcher/CROSS_PLATFORM.md`, so it works with no speakers
  attached) via `pipewire-rs`, sent to the host and played to its output
  sink.
  - The null sink lives only as long as niox-session, so if
    niox-session crashes or stops, the client's audio falls back to its
    real output instead of going nowhere.
  - An `[audio]` config table (enable, mode, latency target) in
    `niox-config`, handed to niox-session by nioxd and applied on
    reload.

  *Verification*: real system audio from a client with no speakers
  plays on the host. Killing or restarting the client doesn't wedge the
  host. After `kill -9` of the client's niox-session, its local audio
  plays normally again.

- [x] **Audio: multi-client mixing + focus-follow** — one receiver per
  client, a mixer with per-client gain (fixed at 1.0 or 0), and
  follow-focus vs. always-mix driven by focus events from nioxd. Each
  client already gets its own receiver and PipeWire playback stream
  (PipeWire does the mixing), and `mode` already reaches niox-session.
  *Verification*: two clients mix cleanly. In follow-focus mode, cycling
  focus mutes and unmutes the right streams.

- [x] **Audio packaging** — niox-session and its user unit in the RPM
  and `.deb`. libroc becomes a runtime dependency, so put niox-session
  in its own subpackage and keep KM-only installs free of
  `roc-toolkit`. Document enabling it under a lingering user on a
  headless host.
  *Verification*: on fresh Fedora and Debian installs, audio works
  after a reboot and login. A KM-only install doesn't pull in libroc.

## Tray + settings window

The desktop control surface replacing NKMS's applet and settings
window: a tray icon in niox-session and an egui settings window, both
built on nioxd's control socket (see ARCHITECTURE.md "Control plane").
There's no network-facing control. A headless machine, or one at the
login screen, is configured with `nioxctl` (over SSH if need be) or by
editing the config file.

- [ ] **Control API: config edits** — nioxd edits its own config on
  request, so no front end ever writes the file:
  - Requests on the control socket to read the editable config (no
    private key) and to add, change, or remove a peer and set `[km]` and
    `[audio]` fields. Each edit carries the revision it was made
    against; nioxd rejects it if the file has changed since (a hand edit,
    another front end).
  - nioxd applies an edit to the file as written (`toml_edit`, keeping
    comments and layout), validates the result with `niox-config`,
    writes it atomically, then reloads as if sent SIGHUP. A failed
    validation leaves the file untouched and names the field, as at
    load.
  - `nioxctl` subcommands covering the same edits (e.g. `nioxctl peer
    add|set|remove`, `nioxctl set audio.mode follow-focus`).

  *Verification*: integration tests over a temp socket: an edit
  round-trips with comments intact, an invalid edit and a stale one are
  both refused without touching the file. On the VMs, pair a new client
  using only `nioxctl peer add` on both ends, with no restart, and a
  user outside the `niox` group is refused.

- [ ] **Control API: hotkey capture** — a request that has nioxd report
  the next key combination pressed on any of its keyboards, as evdev
  codes, so front ends never translate toolkit key names. The current
  hotkey doesn't switch focus while a capture is running, and a capture
  ends on a timeout or when its connection closes. `nioxctl hotkey
  capture` prints the result, which `nioxctl set km.hotkey` applies.
  *Verification*: on a VM driven with `virsh send-key`, a capture
  returns the pressed codes, pressing the current hotkey during it
  doesn't move focus, and the captured hotkey switches focus once set.

- [ ] **Tray icon** — a StatusNotifierItem in niox-session via `ksni`
  (D-Bus only, no GUI toolkit):
  - The icon and tooltip show where focus is (host or which client) and
    whether nioxd is reachable. On a client, it shows whether this
    machine has focus.
  - Menu: focus targets with their connection state; on a host,
    per-client mute (runtime only, through niox-session's mixer) and the
    follow-focus toggle (an `[audio] mode` edit through nioxd); and
    "Settings…", which launches `niox-settings`.
  - Icons ship in the hicolor theme with niox-session's subpackage. With
    no tray host (stock GNOME, a lingering user), niox-session runs as
    before, and the icon appears once a tray host does.

  *Verification*: on the KDE VM, switch focus, mute a client, and toggle
  follow-focus from the tray. Restarting plasmashell brings the icon
  back, stopping nioxd shows it as unreachable until it returns, and
  `kill -9` of niox-session never affects KM.

- [ ] **Settings window** (`niox-settings`) — a separate binary using
  egui (`eframe`, glow backend), launched from the tray, talking to
  nioxd's socket directly:
  - Sections for this machine (public key with copy, port), peers (add,
    edit, remove), the hotkey (set by capture), and audio (enable, mode,
    latency). nioxd's validation errors show next to the field.
  - Subscribes for status and repaints on each event
    (`request_repaint`), so it stays current while open, including
    after hand edits.
  - One instance at a time (a lock in `$XDG_RUNTIME_DIR`; a second
    launch just exits). Wayland app_id `niox-settings` with a matching
    `.desktop` file and icon. System fonts loaded as a fallback so
    non-Latin peer names render.
  - Its own subpackage, so machines without a desktop don't pull in its
    dependencies. UI tests with `egui_kittest` in `cargo test`.

  *Verification*: on the VMs, pair a new client entirely from the
  window, change the hotkey by capture and use it, and toggle the audio
  mode. Hand-edit the config while the window is open and confirm it
  refreshes, and that saving a change made before the hand edit is
  refused. Installs from both the RPM and the `.deb` and launches from
  the desktop menu.

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
- Per-client audio volume beyond mute.
- Network-facing control (an HTTP API or web UI for a phone or a
  headless host). Config edits can add peers, which hands that peer
  this machine's input, so a LAN listener needs real auth and TLS, not
  just a token. If it returns, consider limiting it to status, focus,
  and mute.

## Bugfixes

Known bugs that aren't part of a step above. Fixes to shipped behavior
or its tests land on master and are merged into develop. Pick these up
between steps.

- [x] **Flaky transport endpoint tests: port reuse race** — in
  `crates/niox-transport/tests/endpoint.rs`, `Node::new` binds port 0 to
  pick a free port, records it, and drops the socket. `Node::start` binds
  that port again later. In between, another test running in parallel
  can take it, and the bind's `unwrap()` panics. Seen once in
  `learns_a_peers_address_from_authenticated_traffic` during a full
  `cargo test`, while it passed 15 out of 15 runs on its own. Fix it by
  keeping the bound socket in `Node` and handing it to `start` rather
  than rebinding.
  *Verification*: the workspace's `cargo test` passes 20 runs in a row,
  and nothing in the tests binds a port it picked earlier.

- [x] **Audio: lip sync on clients** — video played on a client ran
  ahead of its audio by the ~200ms the audio takes to reach the host,
  because the niox sink reported no latency of its own and players had
  nothing to hold video back by. The sink's port now carries a
  `Latency` param, which PipeWire adds to the delay each stream playing
  into it sees (pipewire-pulse's `pa_stream_get_latency` included).
  It starts at the latency target and follows Roc's measured end-to-end
  latency once the host's feedback reports it.
  *Verification*: on the VMs, a pulse client playing into the niox sink
  sees about 220ms more latency than on a null sink (46ms), and the
  value moves to the measured latency once it's in.

- [x] **Audio: lip sync counts the host's output** — Roc's end-to-end
  latency stops where the host's player reads the decoder, so what the
  sink reports leaves out the host's own playback path (its graph
  quantum and output device, tens of ms; more on a Bluetooth output).
  Have the host measure its player's delay (`pw_stream_get_time_n`) and
  send it back to the client, on a new audio channel byte or in the
  control stream, and add it to the sink's latency.
  *Verification*: with a Bluetooth or other slow output on the host, a
  client's reported latency grows by about that output's latency, and a
  lip-sync test video (a clap or beep test) lines up within ~20ms.
  On the VMs, the host reported its 21ms quantum, and forcing an
  8192-frame quantum (170ms) was reported in seconds both ways; the
  client's latency settled at 280ms, up from 179ms (Roc's own queue
  settled lower). Checked on real machines with 0.2.1.

