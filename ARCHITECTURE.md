# niox — Network I/O Exchange

## Purpose

niox lets a single **host** machine — the one with your keyboard, mouse, and
(eventually) other physical I/O plugged in — control and exchange input,
audio, and eventually video with one or more **client** machines over the
LAN. It's the successor to two earlier projects, both kept under `old/` for
reference:

- **NKMS** (`old/nkms`, Python) — one host drives many clients' keyboard and
  mouse, cycling focus between them on a hotkey.
- **audio-switcher** (`old/audio-switcher`, Go) — many clients' audio mixes
  together onto one hub's speakers.

niox unifies these into one system, in one language, with a shared security
and control model, and leaves room to add video/graphics later without
another rewrite.

Unlike compositor-level KVM tools (Deskflow/Input Leap, lan-mouse), niox
works at the evdev/uinput layer. That makes it independent of X11 vs.
Wayland and of the desktop environment, and lets it work at the login
screen before anyone has logged in.

## Terminology

- **Host** — the machine with the physical keyboard/mouse (and later,
  screen) attached; the thing you're actually sitting at.
- **Client** — a machine being controlled/fed by the host.
- **Focus** — which client (or the host itself) currently owns the
  keyboard/mouse, cycled via hotkey exactly as in NKMS today.
- **Subsystem** — one data type niox moves across the network: `km`
  (keyboard/mouse), `audio`, and later `video`.

## Design principles carried forward

- **Manual configuration, no discovery/signaling machinery.** Both old
  projects deliberately chose manual IP config over mDNS/auto-discovery, and
  audio-switcher's cross-platform research explicitly favored
  per-platform custom transports over WebRTC to avoid ICE/SDP complexity.
  niox keeps that bias: config files list peers by address, nothing is
  auto-discovered.
- **The input hot path never crosses a privilege or IPC boundary.** NKMS
  already gets this right by running capture-and-send in one root process;
  niox preserves it explicitly (see "Privilege model" below) even as more
  subsystems and an IPC layer are added.
- **KM never depends on a user session.** Keyboard/mouse must work at the
  login screen and on a headless host with nobody logged in, as NKMS's
  system service does today. Anything that needs a user session (audio,
  later video) is layered on top and optional.
- **No infrastructure beyond the niox binaries themselves.** Encryption,
  control, and UI are all embedded rather than depending on an external
  daemon, tunnel, or virtual network interface — important for headless
  hosts.

## Components

```
┌─────────────────────────── host machine ────────────────────────────┐
│                                                                      │
│  ┌─────────────────────┐   control (UDS,     ┌─────────────────────┐ │
│  │  nioxd              │   low-frequency)    │  niox-session       │ │
│  │  (root, system unit)│◄────────────────────│  (user session,     │ │
│  │                     │  session connects;  │   optional)         │ │
│  │  - owns config      │  nioxd runs fine    │                     │ │
│  │  - evdev capture    │  without it         │  - audio capture/   │ │
│  │  - hotkey/focus     │                     │    mix/mux          │ │
│  │    detection        │                     │  - reacts to focus  │ │
│  │  - Noise-encrypted  │                     │    events           │ │
│  │    UDP send         │                     │  - HTTP control API │ │
│  │    (KM hot path)    │                     │    + web UI         │ │
│  └─────────┬───────────┘                     └─────────┬───────────┘ │
└────────────┼───────────────────────────────────────────┼─────────────┘
             │ KM events (Noise/UDP)            audio (Roc packets,
             │                                   Noise-wrapped)
             ▼                                           ▼
┌─────────────────────────── client machine ───────────────────────────┐
│  ┌─────────────────────┐   control (UDS)     ┌─────────────────────┐ │
│  │  nioxd              │◄────────────────────│  niox-session       │ │
│  │  (root, system unit)│                     │  (user session,     │ │
│  │  - owns config      │                     │   optional)         │ │
│  │  - Noise-encrypted  │                     │  - audio capture    │ │
│  │    UDP receive      │                     │  - HTTP control API │ │
│  │  - uinput write     │                     │    + web UI         │ │
│  │    (KM hot path)    │                     │                     │ │
│  └─────────────────────┘                     └─────────────────────┘ │
└──────────────────────────────────────────────────────────────────────┘
```

Every machine runs `nioxd`. Role is config/runtime state, not a
build-time distinction, matching NKMS's existing `Mode: Server|Client`
approach. `niox-session` is only needed for the features that require a
user session.

### Privilege model (Linux)

- `nioxd` runs as root under a **system** systemd unit, started at
  boot, so KM works before login (typing the password on a client's login
  screen) and on headless hosts.
- `niox-session` runs unprivileged under a `systemctl --user` unit, because
  audio needs the logged-in user's PipeWire session. On a headless host
  that wants niox-session features, it runs under a lingering user
  (`loginctl enable-linger`).
- nioxd never depends on niox-session. If niox-session is missing,
  crashed, or restarting, KM is unaffected.

### `nioxd` (privileged)

- The only component that needs elevated privileges: `evdev` device access
  and `uinput` device creation.
- **Owns the config file** (`/etc/niox/niox.toml`, root-owned `0600`),
  including the machine's identity keypair. Applies changes on reload
  (SIGHUP / `systemctl reload`) without dropping sessions unnecessarily.
- On the sending side (host, KM-focused on some client): grabs input
  devices, detects the switch hotkey, and **sends the resulting event
  stream directly** over an encrypted UDP socket. On the receiving side
  (client): receives that stream and writes it to a local `uinput` device.
- Deliberately fat: capture/receive, encryption, and network I/O all live
  in this one process so the KM hot path never makes a local IPC hop.
- Serves the local control socket: publishes focus changes and status to
  any connected niox-session or nioxctl, and accepts low-frequency control requests
  (switch focus, reload, config edits).

### `niox-session` (user session, optional)

- Owns everything that needs the user's desktop/audio session rather than
  root: audio capture/playback (PipeWire/PulseAudio), and later video
  capture/encode.
- Subscribes to nioxd's **focus events** and reacts by adjusting the
  audio mixer (mute non-focused clients in "follow focus" mode, or leave
  everything mixed in "always mix" mode — selected in config).
- Runs the HTTP control API and serves the web UI (see below). Config
  edits are sent to nioxd over IPC rather than written directly.

### Local IPC (nioxd ↔ niox-session / nioxctl)

- Unix domain socket at `/run/niox/niox.sock`. **nioxd listens**;
  `niox-session` and the `nioxctl` CLI connect as clients.
- Access is restricted by file permissions: root-owned, group `niox`,
  mode `0660`. Membership in the `niox` group is the access boundary.
  Peer credentials (`SO_PEERCRED`) can further restrict which requests a
  given caller may make.
- Protocol: newline-delimited JSON, matching the style NKMS already uses
  for its network protocol. Message volume is low (focus changes, config
  reloads, status) so there's no reason to reach for a binary format or
  protobuf here.
- **Never carries per-event KM data.** If a future change would put
  individual mouse/key events on this channel, that's a sign the design
  has drifted from the "hot path stays inside the privileged process"
  principle and should be reconsidered.

## Network protocol & security

- **Transport**: UDP for KM (unreliable, low-latency — a dropped or
  stale mouse-move should never be retransmitted). Audio uses Roc
  (FEC, clock-drift correction) for encoding/decoding, with its packets
  carried over niox's own encrypted transport rather than Roc's built-in
  sockets (feasibility to be proven by the "Audio transport spike" step
  in ROADMAP.md). Video, when
  it exists, will pick per-platform based on capture/encode constraints.
- **Encryption/auth**: a Noise protocol handshake (via a vetted library,
  `snow` — not hand-rolled crypto primitives) performed per
  host↔client session, followed by per-packet ChaCha20-Poly1305 AEAD
  encryption. This is the same class of scheme WireGuard/Tailscale/syncthing
  use, but embedded directly in niox's own sockets rather than requiring an
  external tunnel, virtual network interface, or separate daemon.
  Per-packet cost is microseconds of CPU time with zero added round-trips.
- **Session liveness**: sessions carry keepalives. Either side restarting,
  or a network drop, is detected and recovered by an automatic
  re-handshake using the same configured keys. No re-pairing is needed,
  since trust lives in config, not in session state.
- **Control messages that must arrive** (e.g. the capabilities exchange)
  are acknowledged and resent. Per-event KM traffic is not.
- Each machine has a keypair; a client trusts a host (and vice versa) by
  having its public key listed in the peer's config — manual trust,
  consistent with the "no auto-discovery" principle above.

## Control plane

- Before the web UI exists, `nioxctl` (talking to the nioxd socket) and
  `systemctl reload` are the control surface.
- The HTTP server and web UI are the **primary** long-term control
  surface, not a system tray, because they work on headless hosts where
  a tray icon can't exist.
- Bound to the LAN interface (not just localhost), so it's reachable from
  a phone or other device on the network. Because of that, it requires its
  own lightweight auth (a token/password, separate from the Noise
  handshake used for the KM/audio wire protocol — this secures *access to
  the control plane*, not the media streams).
- A tray icon may still be added later as optional convenience on desktop
  machines, but niox must be fully operable with it entirely absent.

## Repo structure

Single Rust Cargo workspace. niox was first prototyped in Go, through
single-client KM; that code is archived in `archive/go/` as a reference.
Rust was chosen mainly for the audio and video ecosystems (PipeWire,
GStreamer, real-time audio with no GC).

```
niox/
  Cargo.toml           # workspace root
  crates/
    nioxd/             # bin: privileged system service — config owner, KM capture/send/receive/uinput
    niox-session/      # bin: user-session process — audio, control API, web UI
    nioxctl/           # bin: CLI over the nioxd socket (status, reload, pubkey)
    niox-km/           # evdev/uinput capture + virtual input, hotkey/focus logic
    niox-audio/        # capture, Roc encode/decode, mixer, focus-follow logic
    niox-transport/    # Noise handshake + AEAD-wrapped UDP, shared by subsystems
    niox-ipc/          # UDS control channel (nioxd <-> niox-session/nioxctl)
    niox-control/      # HTTP API + web UI (assets embedded in the binary)
    niox-config/       # shared TOML config loading
  web/                 # web UI static assets/frontend source
  archive/go/          # archived Go prototype, reference only
  old/                 # NKMS and audio-switcher, kept for reference
```

Crates are created when the roadmap reaches them; video gets a crate
once it has a design.

## Implementation conventions

- **Concurrency: plain OS threads and channels, no async runtime** in
  nioxd. The KM hot path is a handful of blocking loops (evdev read →
  encrypt → UDP send; UDP recv → decrypt → uinput write), each best served
  by a dedicated thread that blocks on its fd with no scheduler in
  between. Low-frequency work (the control socket, timers, signal
  handling) also gets threads, talking over `std::sync::mpsc` or
  crossbeam channels. An async runtime may be used where it earns its
  keep off the hot path (e.g. the HTTP server in niox-control), but never
  in nioxd's KM threads.
- **Errors**: library crates define typed errors with `thiserror`;
  binaries use `anyhow` and add context at each layer. Per the usual
  convention, an error's `Display` describes only its own layer and
  exposes the cause through `source()`, so `{:#}` prints each layer once.
  Library code doesn't `unwrap()` (a workspace clippy lint); tests may.
- **Logging**: `tracing`. Binaries log to the systemd journal (via
  `tracing-journald`, keeping structured fields and priorities) when
  `JOURNAL_STREAM` is set, and to stderr otherwise. `RUST_LOG` overrides
  the default `info` level.
- **Workspace hygiene**: shared lints and the release profile live in the
  root `Cargo.toml` (`unsafe_code` is denied; a crate that needs FFI
  allows it locally). The release profile keeps unwinding, since Drop
  guards are what release evdev grabs and uinput devices on panic.
  `rust-toolchain.toml` pins Fedora's packaged rustc so local and RPM
  builds match, and `Cargo.lock` is committed.

## Config

- TOML, replacing NKMS's ini-style config and matching audio-switcher's
  existing choice. Loaded and validated by the `niox-config` crate; the
  rest of niox only ever sees a validated `Config`.
- One system config file per machine, `/etc/niox/niox.toml`, owned by
  nioxd (`--config` points it elsewhere, e.g. to run unprivileged against
  a local file during development). niox-session receives the slices it
  needs from nioxd over IPC.
- Layout:

  ```toml
  [identity]            # generated on first run; only the private key is stored
  private_key = "<base64 X25519>"

  [transport]
  port = 4777           # UDP listen port; default port for peer addresses

  [km]
  hotkey = [125, 41]    # evdev codes: hold the first, press the second; one code = single-key

  [[peer]]
  name = "desk"
  address = "192.168.1.20"    # IP or hostname, optional :port
  public_key = "<base64 X25519>"
  role = "client"             # the peer's role relative to this machine: "host" or "client"
  ```

  Later subsystems add their own tables (audio mode, web UI token).
- First run: if the file is missing, nioxd writes a commented default
  with a fresh keypair. If it exists without `[identity]` (e.g. a
  packaged template), one is appended and the rest is left as written.
  Writes are atomic, mode `0600`, and happen only after the rest of the
  file has validated.
- Validation is strict, so mistakes surface at load rather than at
  runtime: unknown fields are rejected, and errors name the field
  (`peer[1] ("desk").role: unknown role "server"; ...`). Peer names,
  public keys, and addresses must each be unique, and no peer may use
  this machine's own key.

## Packaging & deployment

- **Fedora/RPM is the primary target**; `.deb` is kept building as a
  secondary target. Both mirror NKMS's existing `rpm/` and `debian/` setups:
  - `nioxd` — a system unit (root), enabled at boot, analogous to
    `nkms-daemon` today.
  - `niox-session` — a `systemctl --user` unit.
  - A `niox` group, `/etc/niox`, and `/run/niox` (via `RuntimeDirectory=`).
  - Fedora: a firewalld service definition for niox's UDP ports; SELinux
    compatibility for the system unit's evdev/uinput access.
- Platform priority: Linux only for now. macOS, Windows, and mobile are
  deferred (see below).

## Explicitly deferred

- **macOS.** NKMS has a working macOS client that could be ported
  (Quartz virtual input, edge clamping, LaunchAgent). It wouldn't use the
  root-service/user-session split; it would be one user-level process with
  the Accessibility permission.
- **Video/graphics.** Scope and constraints (capture API, encode,
  acceptable latency budget) aren't defined yet. The transport
  abstraction exists to leave room for it, but
  nothing further should be assumed until that design happens separately.
- Clipboard sharing.
- Per-client audio volume controls beyond mute.
- Windows and mobile client support (Roc has no Windows support; mobile
  capture APIs are a different problem per OS — see
  `old/audio-switcher/CROSS_PLATFORM.md`).
- Tray icon convenience layer.

## Open questions to revisit

- Where the HTTP control API runs on a headless host: in `niox-session`
  under a lingering user, or in a separate unprivileged system service
  so the web UI needs no user session at all.
- KM ordering: per-event sequence numbers vs. one datagram per evdev
  SYN frame.
- How audio packets reach the network. nioxd owns the identity keypair
  and the per-peer transport session, but audio lives in the unprivileged
  niox-session. Either niox-session hands Roc packets to nioxd over a
  local socket (one Noise session per peer, but an extra hop for audio),
  or it gets its own Noise session per peer (needs a key it can use).
- Exact focus-event schema between nioxd and the audio mixer.
- Web UI auth mechanism specifics (static token vs. session login).
