# Review findings

Findings too big to fix during the review (see [PLAN.md](PLAN.md)),
grouped by session, most severe first. Each says where, what goes wrong,
and a suggested fix. Strike one through, or delete it, once it's fixed
or moved to ROADMAP.md.

## KM

Fixed during the review (on `develop`):

- `Switcher` desynced permanently on a hotkey key's release without a
  press (a key held past the 2s grab settle) or a second press (the same
  key on two keyboards). After that, every Meta press was judged a
  release. (`a1e3899`)
- `HostKm::peer_up` kept a replacement session's client in the focus
  ring, although its comment said not to rely on a Down first.
  (`d06665e`, defensive: the transport does send Down first.)
- `EV_SW` was defined twice. `web/` was a leftover of the retired web UI.

### KM-1: one unioned virtual device can be misclassified (high, unverified)

`crates/nioxd/src/km/host.rs` (`add_capabilities`) and
`crates/niox-km/src/client.rs` (`on_message`).

The host's passthrough device and each client's virtual device are a
single uinput device with the union of every captured device's
capabilities. udev's `input_id` and libinput classify a device by what
it has. Take a laptop host whose touchpad has `ABS_X`/`ABS_Y`,
`ABS_MT_*`, `BTN_TOOL_FINGER`, and `INPUT_PROP_BUTTONPAD`: it makes the
union look like a touchpad. Its keyboard keys and external mouse's
`REL_*` then go to a device libinput runs through its touchpad dispatch,
which may ignore them, on the host (focus home) and on every client.

Absolute ranges also widen across devices (`Capabilities::insert_abs`
takes the min and max). A touchpad and a tablet sharing `ABS_X` would
then map one of them onto a fraction of the axis.

The test VMs don't show this: their QEMU tablet is an absolute *mouse*
(no `BTN_TOOL_FINGER`), which libinput's fallback dispatch handles
alongside `REL`. NKMS unions the same way, so a desktop host at parity
behaves as before.

- **Verify:** a host with a real touchpad, or a fake one made with
  uinput on a VM. Check `libinput list-devices` and `udevadm info` for
  the `niox virtual input` device, and whether typing and mouse motion
  still work.
- **Fix if confirmed:** one virtual device per device class (keyboard;
  relative pointer; each absolute device), with capabilities unioned only
  within a class. On the wire, give the caps message and each frame a
  device-class byte. That's a wire-format change, so bump the protocol
  version, and fold it into a roadmap step.

### KM-2: hot-plug stalls all input while the passthrough device is recreated (medium)

`crates/nioxd/src/km/host.rs`, `Devices::add` → `State::add_capabilities`.

`VirtualDevice::create` (`UI_DEV_CREATE`, then the kernel and udev
setting up the device) runs with the `host::State` lock held. Every
capture thread waits on that lock, so plugging in a device that grows
the capabilities freezes all input for as long as creation takes.
Destroying the old device also releases everything held on the host.

- **Fix:** build the new `VirtualDevice` before taking the lock, then
  swap it in under the lock. (`set_capabilities` is cheap and can stay
  under it.)

### KM-3: nioxd's scheduling isn't tuned for input latency (medium, measure first)

`dist/nioxd.service`.

The capture threads and the transport driver thread run at default
priority, and `RestrictRealtime=yes` rules out realtime scheduling. On a
host under CPU load (a build, a game), wakeup latency can add
milliseconds per event on both ends.

- **Measure:** event-to-uinput latency under `stress-ng --cpu $(nproc)`,
  with the service as it is and with `Nice=-10` (or
  `CPUSchedulingPolicy=fifo` plus `CPUSchedulingPriority=` and
  `RestrictRealtime=no`). Change the unit only if the difference is
  real.

### KM-4: media keys on separate evdev nodes aren't captured (medium, parity gap)

`crates/niox-km/src/device/discover.rs`, `is_keyboard_or_mouse`.

NKMS's heuristic (`BTN_LEFT`, or both `KEY_A` and `KEY_Z`) skips the
"Consumer Control" and "System Control" nodes most USB keyboards expose
for volume, media, and power keys. Those keys keep acting on the host
even when a client has focus. It's NKMS parity, but likely surprising.

- **Fix:** also capture a node that shares a physical device (same
  `phys` prefix or parent in sysfs) with a captured keyboard and has
  `EV_KEY` codes in the consumer range (`KEY_VOLUMEUP`, `KEY_PLAYPAUSE`,
  …). Leave standalone nodes such as the power button and lid switch
  alone.

### KM-5: per-frame allocations on the capture → send path (low)

`crates/niox-km/src/host.rs`, `process_frame`/`emit`.

Each frame allocates a `current` Vec, a `HostOutput::Local` or `Send`
Vec, and `chunk.to_vec()` for `Message::Frame`. Then `nioxd` encodes the
`Message` into a reused buffer. That's a few hundred nanoseconds against
a network round trip of about 50µs, so not urgent.

- **Fix, if profiling ever shows it:** have `HostOutput::Send` carry the
  frame as a slice range into a reused buffer, or encode frames straight
  into `State::buf`.

### KM-6: the timer thread wakes 10 times a second forever (low)

`crates/nioxd/src/km/host.rs`, `timers`.

It polls every `TICK` (100ms) even when no capabilities are pending, to
catch deadlines set from other threads. That's harmless for latency, but
it's needless wakeups on an idle laptop host.

- **Fix:** a `Condvar` (or the `Stop` pipe plus a second eventfd)
  notified by `State::step` when `next_deadline` moves earlier. The
  thread then sleeps until the deadline, or indefinitely.

### KM-7: status is built in two places (low)

`crates/nioxd/src/main.rs` (`empty_status`) and
`crates/nioxd/src/km/mod.rs` (`Running::status`).

Both build `Status` field by field. The transport handler also finds the
published peer by comparing names, and a rename landing between the
handler's lookup and a reload's republish can drop one update. That's
harmless, since the reload republishes everything.

- **Fix:** one `Status` constructor taking the config plus an optional
  live view. Key published peers by index or public key, not by name.

## Transport

Fixed during the review (on `develop`):

- The driver reset its socket's receive timeout (a `setsockopt`) before
  nearly every datagram, since the wait to the next deadline shrinks
  continuously. A client receiving 1 kHz of KM frames paid a syscall per
  frame. (`84751fa`)
- A resolver stopped by `update_peer` could write the old address after
  the new one landed, and for an IP literal nothing corrected it.
  (`54a3760`)

Checked and found sound: the KK pattern and prologue, the init-timestamp
check (and that a replayed init can't replace a live session, since the
responder's session stays unconfirmed), the replay window's edges and
far jumps, index allocation, `classify`'s length checks, and the locks
on the send path (an uncontended `RwLock` read and a `Mutex` per
datagram, no allocation).

### TR-1: a forged handshake init costs two DHs per configured peer (medium)

`crates/niox-transport/src/endpoint/driver.rs`, `on_init`.

An init carries no peer hint, so the driver trial-decrypts it against
every peer's key, and each `respond` does the `es` and `ss` DHs before
the AEAD check can fail. That's about 120µs per peer on a desktop x86
(release build), and several times that on a Raspberry Pi. Any host on
the LAN can send 64-byte garbage inits: with three peers, about 2,800 a
second (1.4 Mbit/s) fills the driver thread. Real datagrams then queue
behind them and drop, keepalives stop arriving, and every peer is
declared down, so KM stops. Each forged init also allocates the peer
order `Vec` and calls `getrandom` once per peer, which is minor next to
the DHs.

- **Fix:** add a MAC to the init that's checked before any DH, as
  WireGuard's `mac1` is. Key it per peer pair, from the static-static DH
  (`HASH("niox mac" || DH(s_local, s_peer))`) computed once when a peer
  is added. Only the two peers can produce it, and it also names the
  peer, so the trial loop becomes N cheap MACs and at most one
  `respond`. Also cap inits processed per second (a global token
  bucket), so even valid-looking floods are bounded. The init grows by
  16 bytes, a wire-format change, so bump `PROTOCOL_VERSION` and the
  prologue.

### TR-2: sessions never rekey (low)

`crates/niox-transport/src/endpoint/driver.rs`.

A session lives as long as the link, which can be weeks. Anyone who
records the traffic and later reads one session key from memory
decrypts everything since that session began, keystrokes included.
WireGuard bounds this by rekeying every 2 minutes. The static key
alone doesn't expose past sessions (KK's ephemerals see to that), so
this only matters for memory compromise, hence low.

The catch is that today replacing a session reports `Event::Down` then
`Event::Up`, which KM treats as a peer restart (release everything,
resend caps). A rekey needs its own path.

- **Fix:** the initiator of the current session re-handshakes after a
  set age (say 2 minutes) while up. On completion, the new session
  replaces the old one silently. Keep the old one for receiving until
  the first datagram arrives on the new one (or a short grace period),
  so in-flight datagrams aren't lost. Report `Down`/`Up` only for a
  handshake that started while down or came from a restarted peer.

### TR-3: answering an init gives up this side's own pending handshake (low)

`crates/niox-transport/src/endpoint/driver.rs`, `on_init_accepted`.

Answering an init drops `pending` and pushes `retry_at` out by
`dead_after` (3.5s), trusting the peer to finish. But an init that
authenticates isn't necessarily live. After this side restarts,
`newest_init` is empty, so any recorded init from the peer passes, as
does any init this side never saw before (lost, or ignored by the
tie-break). Replaying one while this side is down discards its own init
in flight: the peer's real response to it then finds no pending
handshake and is dropped, and reconnecting waits out the 3.5s. An
attacker with a stock of recorded inits (a new one every 15s while
down, and one per re-handshake) can repeat this once per init, each
newer than the last.

- **Fix:** while down, keep `pending` when answering an init, and take
  whichever handshake completes first: a response to ours, or
  confirmation of theirs. Then drop the other. Check that this can't
  make both sides establish different sessions and flap; the tie-break
  may need to apply here too. At the least, push `retry_at` out by a
  short confirm timeout rather than the full `dead_after`.

### TR-4: replies follow any authenticated datagram, even an old one (low)

`crates/niox-transport/src/endpoint/driver.rs`, `on_data`.

The route moves to the source of any datagram that authenticates,
including one reordered behind newer ones. An attacker who can capture
a datagram and deliver it before the original (or drop the original)
can resend it from another source and redirect this side's replies
there until the next genuine datagram arrives (at most a keepalive
interval, 1s). WireGuard behaves the same way. It's an on-path
attacker, who could drop the traffic anyway, so this is only hardening.

- **Fix:** have `Session::open` report whether the counter was the
  newest yet seen, and roam only on those.
