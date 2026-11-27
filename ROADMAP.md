# niox Roadmap

Working order for building niox out, one verifiable step at a time. Items
are listed in the order to tackle them: the first unchecked item is
what's next. Steps are deliberately unnumbered so they can be added,
split, or reordered without renumbering anything; refer to a step by its
name. Each step should fit in roughly one working session; split any
that turn out bigger. See [ARCHITECTURE.md](ARCHITECTURE.md) for the
design each step implements.

**Current priority: the code review's follow-ups, then the tray icon
and settings window.** Keyboard/mouse has reached Linux parity with
NKMS and replaced it, and audio is done. Fedora is the primary target
OS, with `.deb` packages kept as a secondary target. macOS is deferred
(see below).

## Ground rules

- niox is written in Rust as a single Cargo workspace (layout in
  ARCHITECTURE.md). Crates are created when a step first needs them.
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
- A finished step is checked off where it is and condensed to one line
  naming what it built. Its detail, verification included, is in the
  commit that checked it off.
- Design for the final shape up front where it's cheap: multiple peers,
  nioxd owning its config, and a transport that recovers on its own. No
  step should knowingly build something a later step has to tear out.

## Groundwork

Work that predates this roadmap.

- [x] **Go prototype** — config, a Noise KK transport spike, IPC, and single-client KM in Go; kept as the `go-prototype` tag.

## Keyboard/mouse: road to NKMS parity (Linux)

- [x] **Workspace scaffolding + config** — the workspace, its conventions, and `niox-config`.
- [x] **Transport: handshake + packet layer** — Noise KK via `snow`, datagram formats, and anti-replay.
- [x] **Transport: peer sessions + liveness** — one UDP socket for every peer, with automatic re-handshakes.
- [x] **KM: protocol + focus logic** — wire encoding, the caps exchange, hotkey, focus, and held keys.
- [x] **KM: local devices** — discovery, grabs, capture, and uinput playback.
- [x] **KM end-to-end, single client** — `nioxd` as a root service, working at the login screen.
- [x] **Multi-client + hot-plug** — N clients in the focus cycle, and devices plugged in live.
- [x] **Local control channel + nioxctl** — `niox-ipc` on `/run/niox/niox.sock`, and `nioxctl`.
- [x] **Live config + clean lifecycle** — reload in place, and no leftovers across restarts.
- [x] **Packaging (RPM first, `.deb` second)** — `build.sh`, the spec, `debian/`, and the sandboxed unit.
- [x] ★ **Parity checkpoint: retire NKMS.** niox replaced NKMS on every Linux machine.

## Audio

- [x] **Audio transport spike** — libroc's networkless encoder and decoder inside the transport.
- [x] **niox-session skeleton** — the user-session binary and its user unit.
- [x] **Audio channel through nioxd** — a socketpair per control connection, given to the active seat's user.
- [x] **Audio: one client → host** — the niox sink on the client, played on the host.
- [x] **Audio: multi-client mixing + focus-follow** — one player per client, muted by focus when asked.
- [x] **Audio packaging** — the `niox-session` subpackage, kept apart from KM-only installs.

## Review follow-ups

Everything the whole-project code review
([docs/review/](docs/review/PLAN.md)) found, bugs and larger items
alike, ordered by severity and by what each one unblocks. All of it
comes before the tray and settings window. The review finding each item
came from is named in parentheses.

- [ ] **Reload without `[identity]` re-keys a running nioxd** —
  `niox_config::load` treats a file without `[identity]` as a first run
  on reload too (`Daemon::reload` in `nioxd/src/main.rs`): it writes a
  new keypair into the file and KM restarts under it. Every peer still
  lists the old key, so all of them refuse this machine, the old private
  key is gone, and the reply is just "identity changed". The usual way
  there: replacing `niox.toml` wholesale and running `systemctl reload
  nioxd`. Generate an identity only at startup; on reload, use `parse`
  and refuse a config without `[identity]`, keeping the running one.
  The same write also replaces a symlinked `niox.toml` with a regular
  file, and a `SIGHUP` and a `nioxctl reload` together race on the temp
  name `.niox.toml.tmp`: serialize reloads (`load` runs before the KM
  lock), or make the temp name unique. (CP-1, medium.)
  *Verification*: a test reloads a config without `[identity]` and gets
  an error, with the running identity and the file unchanged.

- [ ] **Hot-plug stalls all input while the passthrough device is
  recreated** — `Devices::add` → `State::add_capabilities`
  (`nioxd/src/km/host.rs`) runs `VirtualDevice::create` (`UI_DEV_CREATE`,
  then the kernel and udev setting the device up) with the
  `host::State` lock held, and every capture thread waits on it. Build
  the new device before taking the lock and swap it in under it.
  (KM-2, medium.)
  *Verification*: on the host VM, input from the existing keyboard keeps
  flowing while a device that adds capabilities is plugged in.

- [ ] **nioxd: reload tests** — nioxd has no tests, and the in-place
  reload (`Running::apply` in `nioxd/src/km/mod.rs`) is the riskiest of
  it: peers matched by public key, new ids routed before the transport
  adds them, the host side starting and stopping, a removed focused
  client released first. Later items here (device classes) and the
  config-edits step change or lean on it, so cover it first. Split the
  reload diff out of `apply` into a pure function (old config and ids,
  new config → peers kept, renamed, re-roled, readdressed, added,
  removed, and whether the host side starts or stops) and unit-test its
  cases. Add a loopback test with two endpoints on `127.0.0.1` for a
  reload that renames and readdresses a connected peer. (Review finding
  XC-1.)
  *Verification*: the new tests cover each kind of peer change, and a
  rename or readdress keeps the peer's session up.

- [ ] **Verify the unioned virtual device on a touchpad host** — a host
  with a touchpad may make libinput treat its single unioned virtual
  device as a touchpad (the fix is the next item, "KM: one virtual
  device per device class"). The VMs' QEMU tablet is an absolute mouse, so they
  don't show it. Check a host with a real touchpad, or fake one with
  uinput (`ABS_MT_*`, `BTN_TOOL_FINGER`, `INPUT_PROP_BUTTONPAD`) on a
  VM: `libinput list-devices` and `udevadm info` for `niox virtual
  input`, and whether typing and mouse motion still work on the host
  and a client. (KM-1, high if confirmed.)

- [ ] **KM: one virtual device per device class** — only if the item
  above, "Verify the unioned virtual device on a touchpad host",
  confirms the problem; otherwise check this off as not needed.
  The host's passthrough device and each client's virtual device union
  every captured device's capabilities, so a touchpad's `ABS_MT_*`,
  `BTN_TOOL_FINGER`, and `INPUT_PROP_BUTTONPAD` would make libinput
  treat the keyboard and mouse sharing it as a touchpad, and absolute
  ranges widen across devices. Make one virtual device per class
  (keyboard; relative pointer; each absolute device), unioning only
  within a class, and give the caps message and each frame a
  device-class byte. That's a KM wire change, so bump its protocol
  version. (KM-1.)
  *Verification*: on a host with a real or uinput-faked touchpad,
  typing, mouse motion, and the touchpad all work on the host and on a
  client, and `libinput list-devices` shows one device per class.

- [ ] **Transport: init MAC and rate limit** — a handshake init names no
  peer, so the driver trial-decrypts it against every peer, two DHs
  each, before the AEAD check fails. About 120µs per peer on a desktop
  (more on a Raspberry Pi): with three peers, ~2,800 garbage 64-byte
  inits a second from any LAN host fill the driver thread, keepalives
  stop, and every peer goes down. Add a MAC to the init, checked before
  any DH, as WireGuard's `mac1` is: keyed per peer pair from the
  static-static DH (`HASH("niox mac" || DH(s_local, s_peer))`, computed
  when a peer is added), so only the two peers can make it and it names
  the peer. The trial loop becomes N cheap MACs and at most one
  `respond`. Also cap inits processed per second with a global token
  bucket. The init grows by 16 bytes, so bump `PROTOCOL_VERSION` and
  the prologue. (TR-1.)
  *Verification*: a flood of forged inits at line rate leaves every
  peer up and KM responsive on the VMs, and a unit test shows a
  wrong-MAC init costs no DH.

- [ ] **Media keys on separate evdev nodes aren't captured** — NKMS's
  heuristic (`is_keyboard_or_mouse` in `niox-km/src/device/discover.rs`:
  `BTN_LEFT`, or `KEY_A` and `KEY_Z`) skips the "Consumer Control" and
  "System Control" nodes most USB keyboards expose, so volume, media,
  and power keys keep acting on the host while a client has focus. Also
  capture a node that shares a physical device (same `phys` prefix or
  sysfs parent) with a captured keyboard and has consumer-range keys
  (`KEY_VOLUMEUP`, `KEY_PLAYPAUSE`, …), leaving standalone nodes such as
  the power button and lid switch alone. (KM-4, medium.)
  *Verification*: on a real USB keyboard, volume keys act on the client
  that has focus; the power button still acts on the host.

- [ ] **Measure KM latency under CPU load** — the capture and driver
  threads run at default priority, and `RestrictRealtime=yes` rules out
  realtime scheduling, so a busy host or client may add milliseconds
  per event. Measure event-to-uinput latency under `stress-ng --cpu
  $(nproc)` with the unit as it is, with `Nice=-10`, and with
  `CPUSchedulingPolicy=fifo` (plus `CPUSchedulingPriority=` and
  `RestrictRealtime=no`). Change `nioxd.service` only if the difference
  is real. (KM-3, medium if it is.)

- [ ] **Transport: rekeying** — a session lives as long as the link,
  possibly weeks, so one session key read from memory decrypts
  everything recorded since it began (keystrokes included). WireGuard
  rekeys every 2 minutes. The initiator of the current session
  re-handshakes after a set age while up; the new session replaces the
  old silently, the old one keeps receiving until the first datagram
  arrives on the new one (or a short grace period), and `Down`/`Up` are
  reported only for a handshake started while down or from a restarted
  peer, so KM doesn't treat a rekey as a restart. (TR-2.) Fold in
  TR-3 while in that code: answering an init gives up this side's own
  pending handshake and waits out `dead_after` (3.5s), so a replayed
  init while down (any recorded one passes after this side restarts)
  delays reconnecting. While down, keep `pending` and take whichever
  handshake completes first, or at least push `retry_at` out by a short
  confirm timeout.
  *Verification*: with a short rekey age, the VMs rekey repeatedly with
  no KM release or caps resend and no lost frames, and a replayed init
  while down doesn't delay reconnecting.

- [ ] **Audio: a PipeWire restart loses the default output to put
  back** — what the niox sink replaced lives in a `DefaultSink` made
  fresh per PipeWire connection (`niox-audio/src/engine.rs`). After a
  PipeWire restart, WirePlumber restores `default.configured.audio.sink`
  as `niox_sink`, `claim` takes it for an old crash and records nothing,
  and when the sink later goes away the key is cleared, not restored.
  Keep what was replaced in `run`, across sessions: when `claim` finds
  the key naming the niox sink, use the carried value; if it names
  anything else, that's the user's newer choice. (AU-1, low.)
  *Verification*: on a client VM, choose an output, restart PipeWire
  while the host is connected, disconnect the host, and the chosen
  output is the default again.

- [ ] **Audio: a host with no output remakes its player every few
  seconds** (unverified) — with no sink at all, a player's stream never
  links, its decoder never connects, the idle check removes it after
  5s, and the client's next body makes a new one, logging two `info`
  lines each round. Have the idle check tell a player that never
  reached `Streaming` from one whose sender went quiet; log the first
  once at `warn` and back off (say 30s) or wait for a sink to appear.
  (AU-2, low.)
  *Verification*: on a host VM with its sound card removed, a sending
  client logs one warning, not a stream of info lines.

- [ ] **Transport: replies follow an old datagram's source** — the route
  moves to the source of any authenticated datagram, even one reordered
  behind newer ones, so an on-path attacker can resend a captured
  datagram from elsewhere and redirect replies until the next genuine
  one (at most a keepalive, 1s). Have `Session::open` report whether
  the counter is the newest yet seen, and roam only on those (`on_data`
  in `niox-transport/src/endpoint/driver.rs`). (TR-4, low.)
  *Verification*: a unit or endpoint test where a stale datagram from a
  new address doesn't move the route.

- [ ] **One user can take every control connection** — the control
  socket's 64-connection limit (`MAX_CONNECTIONS` in
  `niox-ipc/src/server.rs`) is global and connections have no idle
  timeout, so one `niox` group member can hold them all, locking out
  `nioxctl` and a restarting niox-session. Also cap connections per uid
  (from the `SO_PEERCRED` already read), say 16, exempting root.
  (CP-2, low.)
  *Verification*: an IPC test where one uid's 17th connection is
  refused while another uid still connects.

- [ ] **Sandbox niox-session's user unit** — it feeds peers' audio to
  libroc's C decoder with no hardening at all. Add what a user manager
  supports without breaking PipeWire: `NoNewPrivileges=yes`,
  `LockPersonality=yes`, `RestrictSUIDSGID=yes`,
  `SystemCallArchitectures=native`, `RestrictAddressFamilies=AF_UNIX`,
  and, if PipeWire's modules allow, `MemoryDenyWriteExecute=yes` and
  `RestrictNamespaces=yes`. Not `RestrictRealtime=`: PipeWire clients
  may take realtime scheduling. (XC-2, low.)
  *Verification*: audio both ways on the VMs with the new unit, and
  `systemd-analyze --user security niox-session` improves.

- [ ] **Review cleanups** — small items that aren't bugs:
  - KM's timer thread wakes every 100ms forever, even with no
    capabilities pending. Use a `Condvar` (or a second eventfd next to
    the `Stop` pipe) that `State::step` notifies when `next_deadline`
    moves earlier, and sleep until the deadline. (KM-6.)
  - `Status` is built field by field in `nioxd/src/main.rs`
    (`empty_status`) and `km/mod.rs` (`Running::status`). Make one
    constructor taking the config and an optional live view, and key
    published peers by index or public key, not name. (KM-7.)
  - nioxd links two X25519 implementations and two `getrandom`s
    (niox-config's `x25519-dalek` 3 and `getrandom` 0.4, also used by
    niox-transport, against `snow`'s `curve25519-dalek` 4 and
    `getrandom` 0.3), and niox-roc's `bindgen` 0.73 duplicates
    pipewire-rs's 0.72. Align them with `snow` and pipewire-rs until
    those move on, and run `cargo audit` before each release. (XC-3.)
  - Only if profiling ever shows it: the capture → send path allocates
    three Vecs per frame (`current`, the `HostOutput`, `chunk.to_vec()`),
    a few hundred ns against a ~50µs round trip. (KM-5.)

  *Verification*: `cargo tree -d` no longer lists `curve25519-dalek`,
  `getrandom`, or `bindgen`; an idle nioxd host shows no periodic
  wakeups (`perf trace` or `strace -f`).

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
  a working macOS client to port
  ([`virtual_input_macos.py`](https://github.com/nicholasyoder/nkms/blob/6f889581396030e5839d5aef255ad018a72ea41c/nkms/core/virtual_input_macos.py):
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

Known bugs that aren't part of a step above. Fixes land on develop like
everything else. Pick these up between steps.

- [x] **Flaky transport endpoint tests: port reuse race** — tests keep the socket they bound instead of rebinding its port.
- [x] **Audio: lip sync on clients** — the niox sink reports its latency so players can hold video back.
- [x] **Audio: lip sync counts the host's output** — the host sends back its playback delay, and the sink adds it.
