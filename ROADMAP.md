# niox Roadmap

Working order for building niox out, one verifiable milestone at a time.
See [ARCHITECTURE.md](ARCHITECTURE.md) for the design each milestone
implements.

**Current priority: full Linux parity with NKMS, then retire NKMS.**
Everything up to the parity checkpoint is keyboard/mouse only. Audio,
the web UI, and other new capabilities come after it. Fedora is the
primary target OS, with `.deb` packages kept as a secondary target.
macOS is deferred (see below).

## Done

- [x] **0. Project scaffolding** — Go module, package skeleton,
  `ARCHITECTURE.md`, git init.

- [x] **1. Config foundation** (`internal/config`) — TOML load/save, a
  per-machine identity keypair (generated on first run), and a peer list
  (address + public key).

- [x] **2. Transport spike** (`internal/transport`) — Noise KK handshake
  + per-packet ChaCha20-Poly1305 AEAD over UDP, with anti-replay.
  Deliberately spike-scoped: one handshake attempt, no keepalive, no
  re-handshake (picked up in milestone 6).

- [x] **3. Local control channel** (`internal/ipc`) — Unix domain socket,
  newline-delimited JSON, permission-restricted socket file.

- [x] **4. KM end-to-end, single client** (`internal/km`) — evdev
  capture, hotkey/focus switching, and uinput playback ported from NKMS,
  running over the encrypted transport. In this milestone nioxd
  takes its config from niox-session over IPC, which milestone 5 reverses.

## Road to NKMS parity (Linux)

- [ ] **5. nioxd independence** — make `nioxd` a standalone
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

- [ ] **6. KM hardening** — the robustness NKMS earned the hard way, done
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

- [ ] **7. Multi-client + hot-plug** — N clients with NKMS's cycling
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

- [ ] **8. Live config + clean lifecycle** — config changes take effect
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

- [ ] **9. Packaging (RPM first, `.deb` second)** — one build producing
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
  that niox doesn't becomes a bug against milestones 5–9 before moving
  on.

## After parity

- [ ] **10. Control API + web UI** (`internal/control`) — HTTP status
  (focus, peers) and control (switch focus, start/stop, edit
  config via nioxd), LAN-reachable with token auth, usable from a
  phone. Must work on a headless host. Decide first where the HTTP
  server runs (see ARCHITECTURE.md open questions).
  *Verification*: switch focus and edit a peer from a phone browser,
  against a host with nobody logged in.

- [ ] **11. Audio transport spike** — prove that Roc can run inside the
  Noise transport before building on it. Roc normally owns its own UDP
  sockets, so this means using Roc's packet-level encoder/decoder API
  (checking whether `roc-go` exposes it, or binding it ourselves) and
  carrying its packets over `transport.Conn`. If that isn't workable,
  pick and document the alternative explicitly (e.g. Opus over our own
  transport).
  *Verification*: test tone from one Fedora machine to another,
  encrypted, with Fedora's packaged `roc-toolkit` 0.4.

- [ ] **12. Audio: one client → host** — capture from a dedicated
  PipeWire null sink set as the client's default (per
  `old/audio-switcher/CROSS_PLATFORM.md`, so it works with no speakers
  attached), sent to the host and played to its output sink. This is
  mostly new work: audio-switcher only reached its milestone 1.
  *Verification*: real system audio from a client with no speakers
  plays on the host. Killing or restarting the client doesn't wedge the
  host.

- [ ] **13. Audio: multi-client mixing + focus-follow** — one receiver
  per client, a mixer with per-client gain (fixed at 1.0 or 0), and
  follow-focus vs. always-mix driven by focus events from nioxd.
  *Verification*: two clients mix cleanly. In follow-focus mode, cycling
  focus mutes and unmutes the right streams.

- [ ] **14. Web UI: audio controls** — mute state, focus-follow toggle,
  and per-client presence in the milestone 10 UI.
  *Verification*: exercise KM + audio entirely from the web UI on a
  headless host.

## Explicitly deferred

Tracked as future work, not scheduled into the milestones above:

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
