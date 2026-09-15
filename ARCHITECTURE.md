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
- **No infrastructure beyond the niox binary itself.** Encryption,
  control, and UI are all embedded rather than depending on an external
  daemon, tunnel, or virtual network interface — important since a target
  deployment is a headless Raspberry Pi.

## Components

```
┌─────────────────────────── host machine ───────────────────────────┐
│                                                                     │
│  ┌────────────────────┐   control (UDS,     ┌────────────────────┐│
│  │  input-helper       │   low-frequency)    │  niox-daemon        ││
│  │  (root)             │◄───────────────────►│  (user session)     ││
│  │                     │                     │                     ││
│  │  - evdev capture    │                     │  - audio capture/   ││
│  │  - hotkey/focus     │                     │    mix/mux          ││
│  │    detection        │                     │  - focus event bus  ││
│  │  - Noise-encrypted  │                     │  - HTTP control API ││
│  │    UDP send         │                     │    + web UI         ││
│  │    (KM hot path)    │                     │  - config           ││
│  └─────────┬───────────┘                     └─────────┬───────────┘│
└────────────┼─────────────────────────────────────────────┼──────────┘
             │ KM events (Noise/UDP)              audio (Roc/RTP,
             │                                     Noise-wrapped)
             ▼                                              ▼
┌─────────────────────────── client machine ──────────────────────────┐
│  ┌────────────────────┐   control (UDS)     ┌────────────────────┐ │
│  │  input-helper       │◄───────────────────►│  niox-daemon        │ │
│  │  (root)             │                     │  (user session)     │ │
│  │  - Noise-encrypted  │                     │  - audio playback    │ │
│  │    UDP receive      │                     │  - HTTP control API  │ │
│  │  - uinput write     │                     │    + web UI          │ │
│  │    (KM hot path)    │                     │  - config             │ │
│  └────────────────────┘                     └────────────────────┘ │
└──────────────────────────────────────────────────────────────────────┘
```

Every machine runs both processes regardless of whether it's currently
acting as host or client for a given subsystem — role is config/runtime
state, not a build-time distinction, matching NKMS's existing
`Mode: Server|Client` approach.

### `input-helper` (privileged)

- The only component that needs elevated privileges: `evdev` device access
  and `uinput` device creation.
- On the sending side (host, KM-focused on some client): grabs input
  devices, detects the switch hotkey, and **sends the resulting event
  stream directly** over an encrypted UDP socket. On the receiving side
  (client): receives that stream and writes it to a local `uinput` device.
- Deliberately fat: capture/receive, encryption, and network I/O all live
  in this one process so the KM hot path never makes a local IPC hop.
- Talks to `niox-daemon` only for **control**: current focus target,
  capability list changes (devices plugged/unplugged), start/stop,
  hotkey config changes — all low-frequency, all over the local IPC
  channel.

### `niox-daemon` (user session)

- Owns everything that needs the user's desktop/audio session rather than
  root: audio capture/playback (PipeWire/PulseAudio), and later video
  capture/encode.
- Hosts the **focus event bus** — when `input-helper` reports a focus
  change, `niox-daemon` reacts by adjusting the audio mixer (mute
  non-focused clients in "follow focus" mode, or leave everything mixed in
  "always mix" mode — both are supported, selected in config).
- Runs the HTTP control API and serves the web UI (see below).
- Reads/writes the shared TOML config file and pushes relevant changes down
  to `input-helper` over IPC.

### Local IPC (`input-helper` ↔ `niox-daemon`)

- Unix domain socket. Available natively on Linux, macOS, and Windows
  (`AF_UNIX` since Windows 10 1803) — one implementation across all target
  platforms, unlike NKMS's D-Bus (Linux-only).
- Socket file permissions restrict the connection to the `niox-daemon`
  process's user, giving a natural access boundary at the privilege
  boundary we already care about.
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
  stale mouse-move should never be retransmitted) and Roc/RTP for audio
  (as in audio-switcher today, FEC included). Video, when it exists, will
  pick per-platform based on capture/encode constraints, not necessarily
  the same transport as KM/audio.
- **Encryption/auth**: a Noise protocol handshake (via a vetted library,
  e.g. `flynn/noise` — not hand-rolled crypto primitives) performed once
  per host↔client connection, followed by per-packet
  ChaCha20-Poly1305 AEAD encryption. This is the same class of scheme
  WireGuard/Tailscale/syncthing use, but embedded directly in niox's own
  sockets rather than requiring an external tunnel, virtual network
  interface, or separate daemon. Per-packet cost is microseconds of CPU
  time with zero added round-trips — it does not introduce the kind of
  latency that would make mouse input feel laggy.
- Each machine has a keypair; a client trusts a host (and vice versa) by
  having its public key listed in the peer's config — manual trust,
  consistent with the "no auto-discovery" principle above.

## Control plane: HTTP + web UI

- The HTTP server and web UI are the **primary** control surface, not a
  system tray. This is required for headless deployments (e.g. an
  always-on Raspberry Pi host with no desktop session) — a tray icon
  can't exist there, but the web UI works identically everywhere.
- Bound to the LAN interface (not just localhost), so it's reachable from
  a phone or other device on the network. Because of that, it requires its
  own lightweight auth (a token/password, separate from the Noise
  handshake used for the KM/audio wire protocol — this secures *access to
  the control plane*, not the media streams).
- A tray icon may still be added later as optional convenience on desktop
  machines (open-browser-to-the-UI, quick start/stop), but niox must be
  fully operable with it entirely absent.

## Repo structure

Single Go monorepo:

```
niox/
  cmd/
    niox-helper/     # privileged binary: KM capture/send/receive/uinput
    niox-daemon/      # user-session binary: audio, control API, web UI
  internal/
    km/               # evdev/uinput capture + virtual input, per-OS
    audio/             # capture, Roc transport, mixer, focus-follow logic
    video/              # not yet implemented — placeholder for future work
    transport/           # Noise handshake + AEAD-wrapped UDP, shared by subsystems
    ipc/                   # UDS control channel (helper <-> daemon)
    control/                # HTTP API + web UI (embed.FS)
    config/                  # shared TOML config loading
  web/                        # web UI static assets/frontend source
  old/                          # NKMS and audio-switcher, kept for reference
```

## Config

- TOML, replacing NKMS's ini-style config and matching audio-switcher's
  existing choice.
- One config file per machine, listing: this machine's identity/keypair,
  known peers (address + public key), per-subsystem role/settings (KM
  hotkeys, audio mode — follow-focus vs. always-mix — client port
  assignments), and web UI auth token.

## Packaging & deployment

- `.deb` + systemd, mirroring both old projects:
  - `niox-helper` — a system-level unit (root), analogous to
    `nkms-daemon` today.
  - `niox-daemon` — a `systemctl --user` unit, analogous to
    audio-switcher's planned user service.
- Platform priority: Linux first (strongest support in both predecessor
  projects), macOS second (both projects have partial precedent — NKMS's
  Quartz-based virtual input, audio-switcher's Roc-via-Homebrew research),
  Windows and mobile deferred (per `old/audio-switcher/CROSS_PLATFORM.md`,
  Roc has no Windows support and mobile capture APIs are a substantially
  different problem per-OS).

## Explicitly deferred

- **Video/graphics.** Scope and constraints (capture API, encode,
  acceptable latency budget) aren't defined yet. The `internal/video`
  package and `transport` abstraction exist to leave room for it, but
  nothing further should be assumed until that design happens separately.
- Per-client audio volume controls beyond mute (audio-switcher already
  scaffolds this with a per-client gain field to avoid a re-architect
  later).
- Windows and mobile client support.
- Tray icon convenience layer.

## Open questions to revisit

- Exact focus-bus event schema between `km` and `audio` packages.
- Web UI auth mechanism specifics (static token vs. session login).
- Whether `input-helper` restarts should silently re-establish the Noise
  session or require re-pairing.
