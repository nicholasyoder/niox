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
