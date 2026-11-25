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
- Learns everything from nioxd: it subscribes on the control socket and
  keeps a view of focus and each peer's name, role, and connection, which
  is the config slice it needs (nioxd's config file is root-only, and
  niox-session never reads it). Later subsystems add their settings to
  the status nioxd publishes (e.g. the `[audio]` table).
- When nioxd isn't there (stopped, restarting, not yet started), it
  retries with backoff (250ms doubling to 5s), logging an outage once,
  and reports whatever changed while it was away when the new snapshot
  arrives. A user outside the `niox` group can't use the socket, so its
  user unit only starts for members (`ConditionGroup=niox`).
- **Audio goes through nioxd.** niox-session never holds a Noise session
  or key: the identity key stays root-only, each peer still has exactly
  one key, port, and session, and audio inherits KM's liveness and
  re-handshakes. niox-session asks for an audio channel on its control
  connection, and nioxd answers by passing it one end of a
  `SOCK_SEQPACKET` socketpair (`SCM_RIGHTS`); each packet on it is one
  `Subsystem::Audio` body behind the peer's configured name
  (`niox_ipc::audio` has the layout), naming the destination going out
  and the source coming in. The channel lasts exactly as long as that
  control connection, so a crashed niox-session frees it with nothing
  left in `/run/niox`. nioxd forwards both ways without ever blocking:
  outbound packets are sealed and sent as `Subsystem::Audio`, and inbound
  ones are written to the channel from the transport's driver thread
  with a non-blocking send, dropped if niox-session has fallen behind
  (Roc conceals the loss). Outbound packets are read on a thread of
  nioxd's own and sent from there. The extra local hop costs microseconds at
  about 300 packets/s, against a latency target of ~200ms.
- **One niox-session gets the audio channel: the active seat's.** More
  than one can be connected (fast user switching, or a lingering user
  plus a desktop login). nioxd identifies each connection's user with
  `SO_PEERCRED` and grants the channel to the user active on `seat0`
  (`ACTIVE_UID=` in `/run/systemd/seats/seat0`, no D-Bus needed),
  moving it when the active user changes. Only that user's PipeWire can
  use the audio devices anyway, since logind gives device access to the
  active session. When the active user has no niox-session asking
  (nobody active, as on a headless host with a lingering user; a login
  screen, whose user runs none; or a user outside the `niox` group), the
  first to ask gets it and keeps it until it disconnects or the active
  user's niox-session asks. nioxd rereads the seat every half second
  while anyone is asking.

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
  protobuf here. Details are in `niox_ipc::protocol`:
  - Requests carry a `cmd` (`status`, `focus`, `subscribe`, `reload`,
    `audio`), and each gets exactly one reply, in order (`status`, `ok`,
    `reloaded` listing what changed, or `error` with a message).
  - `audio` asks for the audio channel and is answered `ok`. Then, for as
    long as the connection is open, an `audio` event says whenever it
    gets the channel (the fd comes with that line) or loses it.
  - `subscribe` replies with a status snapshot, then sends an `event`
    line for every change: `focus` (where focus moved), `peer` (one
    peer's new status), or `status` (the full status, when something
    structural such as the peer list changed). Events and replies share
    the connection; anything that isn't an `event` is a reply. A focus
    target is `"host"` or `{"client": "<name>"}`.
  - Readers ignore fields they don't know, so fields can be added freely;
    the status carries a protocol version for breaking changes.
- nioxd mirrors its status in a `niox_ipc::Publisher` and updates it as
  things happen; the socket answers from that mirror and turns its
  changes into events, so serving it never takes the KM locks. Publishing
  never blocks: a subscriber that falls behind is disconnected. The
  socket is bound under a temporary name and renamed into place once its
  group and mode are set. If the `niox` group doesn't exist it's
  root-only, and if the socket can't be served at all nioxd logs it and
  runs KM anyway.
- **Never carries per-event KM data.** If a future change would put
  individual mouse/key events on this channel, that's a sign the design
  has drifted from the "hot path stays inside the privileged process"
  principle and should be reconsidered.

## Network protocol & security

- **Transport**: UDP for KM (unreliable, low-latency — a dropped or
  stale mouse-move should never be retransmitted). Audio uses Roc
  (FEC, clock-drift correction) for encoding/decoding, with its packets
  carried over niox's own encrypted transport rather than Roc's built-in
  sockets (see "Audio protocol" below). Video, when
  it exists, will pick per-platform based on capture/encode constraints.
- **Encryption/auth**: a Noise protocol handshake (via a vetted library,
  `snow` — not hand-rolled crypto primitives) performed per
  host↔client session, followed by per-packet ChaCha20-Poly1305 AEAD
  encryption. This is the same class of scheme WireGuard/Tailscale/syncthing
  use, but embedded directly in niox's own sockets rather than requiring an
  external tunnel, virtual network interface, or separate daemon.
  Per-packet cost is microseconds of CPU time with zero added round-trips.
- **Wire format** (`niox-transport`; full table in its crate docs):
  datagrams of at most 1280 bytes (the IPv6 minimum MTU) with a
  version/type prefix. A two-message KK handshake exchanges each side's
  session index; the init also carries a timestamp, and a responder only
  accepts an init newer than the last one it accepted from that peer, so
  a captured init can't be replayed. Data datagrams carry the receiver's
  index and an explicit counter (the AEAD nonce), so each decrypts on its
  own whatever was lost or reordered, and a 1984-counter sliding window
  drops duplicates. As in WireGuard, the cleartext header isn't AEAD
  associated data (snow's stateless mode has none); every field is still
  bound, because the counter is the nonce and a changed index selects
  keys that won't open the datagram. The first plaintext byte names the
  subsystem (`km`, `audio`), so all subsystems share one session per
  peer; an empty payload is a keepalive.
- **Sessions and liveness** (`niox_transport::Endpoint`): one UDP socket
  per nioxd (dual-stack) serves every configured peer, and a driver
  thread owns all handshake and timer state. Senders on any thread
  encrypt and send directly on the shared socket; received datagrams are
  decrypted and handed to a handler on the driver thread, with no channel
  hop. Each side sends a keepalive after 1s without sending anything, and
  declares a peer down after 3.5s without hearing from it. While a peer
  is down, both sides send handshake inits with backoff (250ms doubling
  to 2s), so either side restarting, or a network drop, recovers on its
  own using the same configured keys; no re-pairing is needed, since
  trust lives in config, not in session state. A restarted peer
  handshakes on startup, so the other side replaces its stale session
  without waiting for it to time out. The responder treats a new session
  as unconfirmed until the first authenticated datagram arrives on it.
  If both sides initiate at once, the lower public key's init wins.
  Replies follow a peer's last authenticated source address, so a peer
  whose IP changed keeps working. Subsystems get per-peer up/down events.
  Peers can be added, removed, and updated (name, address) while the
  endpoint runs, for config reloads, leaving every other session
  untouched. Peer ids are never reused, so a stale id can't reach a
  different peer. On the test VMs, round trips take about 50µs (p50), and one sending
  thread manages about 1 Gbit/s.
- **KM protocol** (`niox-km`; message layout in its `wire` module):
  - *Ordering.* Each evdev frame (the events up to a `SYN_REPORT`) is
    one datagram, so a frame is never split or reordered internally; the
    receiver appends the `SYN_REPORT`. Frames also carry a per-session
    sequence number. LAN reordering is rare, but when a frame arrives
    after a newer one, the client plays only its key releases: replaying
    a stale press could leave a key stuck down, and a stale motion is
    worthless, while a release can only un-stick a key. Lost frames are
    never resent.
  - *Capabilities.* The host sends the union of its captured devices'
    capabilities (event codes as bitmaps, absolute-axis ranges, input
    properties) to each client when its session comes up and whenever
    they change, resending with backoff (250ms doubling to 2s) until
    acknowledged. A client recreates its virtual device only if the
    capabilities grew, and keeps the union. A client joins the focus
    cycle once it has acknowledged capabilities in the current session.
  - *Focus.* The hotkey state machine is NKMS's, with two fixes (see
    `niox_km::Switcher`). Focus cycles host → each ready client in config
    order → host, tracked by identity so clients joining and leaving
    never move it to a different machine. If the focused client drops,
    focus returns to the host.
  - *Held keys.* The host tracks which keys are down on each target.
    When focus leaves a target, everything held there is released, and
    a key's later repeats and release never reach a target that didn't
    see its press. A client releases everything its host left held when
    the session drops.
  - *Devices* (`niox_km::device`). Keyboards and mice are found by
    NKMS's heuristic (`BTN_LEFT`, or both `KEY_A` and `KEY_Z`), skipping
    niox's own `niox virtual input` devices, and each is grabbed
    exclusively with its own capture thread. Before grabbing, capture
    waits (bounded) for held keys to be released, since a grab swallows
    their releases and would leave them stuck down locally. Capabilities
    cover only what uinput replays: keys, relative and absolute axes,
    `EV_MSC`, switches, and input properties; never `EV_REP`, since the
    source's repeats are forwarded and the virtual device would add its
    own. After a `SYN_DROPPED`, capture discards through the next
    `SYN_REPORT` and emits whatever releases and presses bring its key
    state in line with the kernel's; an unplugged device ends with
    releases for everything it had down. New devices are found by
    listening to the kernel's uevents (`NETLINK_KOBJECT_UEVENT`, the
    stream udevd itself reads), so hot-plug needs neither libudev nor
    udevd; by the time the kernel announces an input device, devtmpfs
    has created its node.
  - *In nioxd* (`crates/nioxd/src/km`). Each peer is handled by its
    configured role: for `client` peers this machine is the host, and
    grabs its keyboards and mice (playing them on a local passthrough
    device while focus is home). A devices thread grabs each at startup
    and as it's plugged in, starting it with no devices at all if need
    be. The capabilities sent to clients, and the passthrough device's,
    are the union of every device seen since startup, so they only grow;
    a device that adds something recreates the passthrough device and
    resends capabilities, and an unplugged device changes nothing. For
    `host` peers it creates
    a virtual device per host once that host's capabilities arrive. A
    machine can be both. On the host, each capture thread runs read →
    hotkey/focus → encrypt → send under one lock shared with the
    transport handler and a timer thread (capabilities resends), so
    frames reach a client in sequence order. On the client, the transport
    driver thread runs receive → decrypt → uinput write. On an orderly
    stop the host first releases whatever is held on the focused client.
  - *Cleanup.* The kernel drops a grab and destroys a uinput device
    when its descriptor closes, so any process exit (even `SIGKILL`)
    cleans up. Within a running process, device threads share a
    `Stop`; a guard triggers it if a thread panics, so a panic never
    leaves other threads holding grabs with nothing replaying them.
- **Audio protocol** (`niox-audio`; the Roc bindings are `niox-roc`):
  - *Roc without its sockets.* Roc 0.4's networkless pipeline
    (`roc_sender_encoder` / `roc_receiver_decoder`), bound with bindgen
    against the system libroc (Fedora's `roc-toolkit`), does everything
    but the network: packetizing, FEC, resampling, clock-drift
    compensation, loss concealment, and latency tuning. Its packets
    travel as `Subsystem::Audio` bodies, so audio gets the transport's
    authentication, encryption, liveness, and re-handshakes, with no
    second socket or key.
  - *Framing.* Each body is one Roc packet behind a channel byte: source,
    repair (FEC), report (the sender's RTCP), or feedback (the
    receiver's RTCP). RTCP flows both ways on one Roc interface, so the
    byte also says which end of the stream a packet is for. niox adds no
    ordering or retransmission of its own; Roc's RTP sequence numbers,
    jitter buffer, and FEC deal with loss and reordering.
  - *Format.* Frames are 32-bit float at any rate; on the wire Roc uses
    its built-in 16-bit PCM at 44.1 kHz and converts both ways. A packet
    must fit one datagram (1246 bytes after the channel byte), which
    caps a stereo packet at about 6.7ms of audio; the default is 5ms
    (902-byte packets). libroc silently drops packets bigger than its
    context's maximum, so an encoder whose packets could exceed it is
    refused up front.
  - *FEC and latency.* Reed-Solomon FEC by default (Roc's 18 source + 10
    repair packets per block). Repairs arrive at the end of a block, so
    the receiver's latency target has to cover one block (90ms at 5ms
    packets) for FEC to help; Roc's default 200ms does.
  - *Restarts.* A restarted sender starts a new RTP stream; the decoder
    drops the old session about 200ms after it goes quiet and picks up
    the new one by itself.
  - On the test VMs, a stereo stream with default FEC measures about
    198ms end to end at Roc's default latency target, and about 317
    datagrams/s (2.4 Mbit/s).
- **Reloading** (`SIGHUP`, i.e. `systemctl reload nioxd`, or `nioxctl
  reload`). nioxd re-reads its config; an invalid one is refused and the
  running config kept. Otherwise the change is applied in place:
  - Peers are matched by public key, the thing a session is with. A peer
    whose key is unchanged keeps its session through a rename, a new
    address, or a role change; a new key is a new peer.
  - KM changes come first, while removed peers' sessions are still up: a
    removed client that has focus is sent releases for everything held
    on it and focus returns to the host, and a removed host's virtual
    device is destroyed. New peers' events are routed (by the id they
    are about to get) before the transport adds them, so their first
    `Up` isn't missed. Then the transport removes, updates, and adds
    peers.
  - The host side starts when the first client is configured and stops
    (releasing its grabs) when the last one is removed. A machine that
    keeps hosting keeps its grabs and focus.
  - A hotkey change takes effect once the old hotkey's keys are both up,
    so a half-finished press is never judged by the new one.
  - A new identity or port can't be applied to a running endpoint, so
    those restart KM as a whole (a new port is bound first, so a taken
    port refuses the reload instead). If applying a reload fails partway,
    nioxd exits and systemd restarts it with the new config.
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
    nioxctl/           # bin: CLI over the nioxd socket (status, pubkey, focus, reload, watch)
    niox-km/           # evdev/uinput capture + virtual input, hotkey/focus logic
    niox-audio/        # capture, Roc encode/decode, mixer, focus-follow logic
    niox-roc/          # bindgen bindings to libroc's encoder/decoder (the only FFI crate)
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
- **System libraries**: `niox-roc` links the system libroc 0.4 (found
  with pkg-config) and generates its bindings at build time, so building
  the workspace needs `roc-toolkit-devel` and `clang-devel` (Debian:
  `libroc-dev`, `libclang-dev`). Only crates that use audio link it;
  nioxd's KM doesn't.

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
  secondary target. `./build.sh` builds both into `target/dist/` from one
  source tarball of the tree; the version comes from the workspace
  `Cargo.toml` (`debian/changelog` must match).
  - RPM (`rpm/niox.spec`): the crates.io dependencies are vendored into a
    second source tarball, so rpmbuild and mock build offline with Fedora's
    cargo.
  - `.deb` (`debian/`): built in a `debian:trixie` container with rustup's
    toolchain (from `rust-toolchain.toml`), since Debian's rustc is older
    than the workspace needs and binaries built on Fedora would need its
    newer glibc.
- Shared install pieces live in `dist/`:
  - `nioxd.service` — a system unit (root), enabled and started on
    install, ordered before the display manager. It's sandboxed
    (`ProtectSystem=strict`, a closed device policy allowing `/dev/uinput`
    and the input device class, so hot-plugged devices work, address
    families limited to `AF_UNIX`/`AF_INET`/`AF_INET6`/`AF_NETLINK`, and
    capabilities limited to `CAP_CHOWN` + `CAP_NET_BIND_SERVICE`).
    `/run/niox` and `/etc/niox` come from `RuntimeDirectory=` and
    `ConfigurationDirectory=`.
  - `niox.sysusers` — the `niox` group, created by rpm (Fedora's
    automatic sysusers handling) and `dh_installsysusers`.
  - `niox.firewalld.xml` — a firewalld service for UDP 4777. It isn't
    enabled automatically: `firewall-cmd --permanent --add-service=niox`.
- `/etc/niox/niox.toml` is not a packaged file: nioxd writes it with a
  fresh keypair on first start, so upgrades and removal never touch it.
  Only `apt purge` deletes it.
- SELinux: on Fedora the packaged nioxd runs as `unconfined_service_t`
  (`bin_t` started by systemd), so it needs no policy module for
  evdev/uinput access.
- `niox-session.service` — niox-session's `systemctl --user` unit
  (`ConditionGroup=niox`, `WantedBy=default.target`). It goes into the
  packages with audio, in its own subpackage so KM-only installs don't
  pull in libroc.
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
- Web UI auth mechanism specifics (static token vs. session login).
