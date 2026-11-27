# Review findings

Findings too big to fix during the review (see [PLAN.md](PLAN.md)),
grouped by session, most severe first. The review is done and every
finding has moved to the *Review follow-ups* section of
[ROADMAP.md](../../ROADMAP.md); each heading below says which item. This
file keeps what each session fixed and what it checked and found sound.

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

Moved to ROADMAP.md, *Review follow-ups*: "Verify the unioned virtual
device on a touchpad host", then "KM: one virtual device per device
class".

### KM-2: hot-plug stalls all input while the passthrough device is recreated (medium)

Moved to ROADMAP.md, *Review follow-ups*: "Hot-plug stalls all input
while the passthrough device is recreated".

### KM-3: nioxd's scheduling isn't tuned for input latency (medium, measure first)

Moved to ROADMAP.md, *Review follow-ups*: "Measure KM latency under CPU
load".

### KM-4: media keys on separate evdev nodes aren't captured (medium, parity gap)

Moved to ROADMAP.md, *Review follow-ups*: "Media keys on separate evdev
nodes aren't captured".

### KM-5: per-frame allocations on the capture → send path (low)

Moved to ROADMAP.md, *Review follow-ups*: "Review cleanups".

### KM-6: the timer thread wakes 10 times a second forever (low)

Moved to ROADMAP.md, *Review follow-ups*: "Review cleanups".

### KM-7: status is built in two places (low)

Moved to ROADMAP.md, *Review follow-ups*: "Review cleanups".

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

Moved to ROADMAP.md, *Review follow-ups*: "Transport: init MAC and rate
limit".

### TR-2: sessions never rekey (low)

Moved to ROADMAP.md, *Review follow-ups*: "Transport: rekeying".

### TR-3: answering an init gives up this side's own pending handshake (low)

Moved to ROADMAP.md, *Review follow-ups*: "Transport: rekeying".

### TR-4: replies follow any authenticated datagram, even an old one (low)

Moved to ROADMAP.md, *Review follow-ups*: "Transport: replies follow an
old datagram's source".

## Control plane

Fixed during the review (on `develop`):

- The config accepted peer names longer than the 255 bytes the audio
  channel can address. Such a peer could never get audio, and nioxd
  logged a failure for every audio packet it received from it.
  (`84ed881`)
- Only the all-zeros peer public key was refused, not the other
  low-order points, which make the static-key DHs zero and so let
  anyone complete a handshake as that peer. (`f821fd2`)
- A comment in `server::serve` credited the wrong thing with ending a
  writer stuck on a client that stopped reading. (`1632ba8`)

Checked and found sound: line-length limits on both ends (`read_line`
caps each line at `MAX_LINE` without buffering more), malformed JSON
(an error reply, and the connection survives), subscribers and audio
waiters that stop reading (dropped with `try_send`, never blocking the
publisher or the arbiter), fds a client sends (closed by the kernel,
since the server reads without a control buffer), the order fds and
lines arrive in on the client, the bind, chmod, and rename sequence
(the socket is only reachable under its final name with its final
mode, and `/run/niox` is root's), the config's atomic write (`0600`
from creation, fsync of the file and the directory), the arbiter's
choice and its retry when a grant can't be queued, and the relay's
inbound path (`try_send` on the driver thread, no allocation beyond an
`Arc` clone). The config and IPC enums (`Role`, `AudioMode`) are
deliberately separate types, with one small mapping each in nioxd; see
KM-7 for the status built twice.

### CP-1: a config without `[identity]` silently re-keys a running nioxd (medium)

Moved to ROADMAP.md, *Review follow-ups*: "Reload without `[identity]`
re-keys a running nioxd".

### CP-2: one user can take every control connection (low)

Moved to ROADMAP.md, *Review follow-ups*: "One user can take every
control connection".

## Audio

Fixed during the review (on `develop`):

- A plan change that remakes a client's receiver (a new latency) could
  leave that client silent until the next plan change: a body arriving
  between `Streams::set_plan` and the queued `Command::Plan` asked for a
  receiver, and `add_player` ignored the ask because the old player was
  still there. (`3fea06d`)
- The sink took any 100ms between process calls for a pause in the
  audio, so with `clock.max-quantum` above 4800 frames, every quantum
  started a new Roc stream. (`f036711`)

Checked and found sound: the `Send`/`Sync` impls (libroc's headers
document the context, encoder, and decoder as safe to use concurrently),
drop order (each encoder and decoder closes before its `Context` clone
drops), buffer sizes on both sides of every call (`pop_into` checks
against the context's maximum, so libroc's single error for "empty" and
"too small" can only mean empty, and it never trusts the returned
length past the buffer), and the log handler. On the PipeWire thread:
stream callbacks never touch the `RefCell` state, so nothing re-borrows
it. The process callbacks reuse their buffers, and their only locks are
short ones in `Streams`, taken in one order throughout (plan, then
receivers, then wanted; senders, then output delays). The thread's
shutdown handshake can't miss a stop: `stop` and the command channel are
set and read in opposite orders. Reconnect backoff is fine on both
sides. A short pause in the audio (150ms to 5s, the host still pulling
silence) comes back within 40ms. A decoder only counts as connected
once frames are popped from it, so a player that never runs is
removed. The plan is derived from the view in one place
(`audio::plan`), and the engine replans only when it changes.

### AU-1: a PipeWire restart loses the default output to put back (low)

Moved to ROADMAP.md, *Review follow-ups*: "Audio: a PipeWire restart
loses the default output to put back".

### AU-2: a host with no output device remakes its player every few seconds (low, unverified)

Moved to ROADMAP.md, *Review follow-ups*: "Audio: a host with no output
remakes its player every few seconds".

## Cross-cutting

Fixed during the review (on `develop`):

- `nioxd.service` now also sets `ProtectProc=invisible`,
  `ProcSubset=pid`, `PrivateIPC=yes`, and `UMask=0077`
  (`systemd-analyze security`: 2.5 to 2.3), checked on the VMs.
  (`acb2d0f`)
- `build.sh` checked `debian/changelog`'s version but not the spec's
  `%changelog`, so an RPM could ship with the last release's entry on
  top. (`8a379d0`)
- ARCHITECTURE.md listed `niox-settings` among the crates. (`cd14698`)

Checked and found sound: the rest of `nioxd.service` against what nioxd
does (every device, address family, and capability it allows is used,
and `ConfigurationDirectory=` stays writable under
`ProtectSystem=strict`), the RPM and `.deb` scriptlets (the `niox`
group exists before nioxd first starts; upgrades restart nioxd and
never touch `/etc/niox`), `build.sh`'s tarball and vendoring, the dev
and test-VM scripts, and the three examples. ARCHITECTURE.md's
constants (timers, backoffs, the replay window, packet sizes) and
`nioxctl`'s commands match the code. `cargo audit` isn't installed
here, so advisories weren't checked.

### XC-1: nioxd has no tests (low)

Moved to ROADMAP.md, *Review follow-ups*: "nioxd: reload tests".

### XC-2: niox-session's user unit isn't sandboxed (low)

Moved to ROADMAP.md, *Review follow-ups*: "Sandbox niox-session's user
unit".

### XC-3: duplicate crypto and build dependencies (low)

Moved to ROADMAP.md, *Review follow-ups*: "Review cleanups".
