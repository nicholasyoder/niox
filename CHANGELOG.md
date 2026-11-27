# Changelog

Notable changes to niox, newest first. Versions follow
[Semantic Versioning](https://semver.org/); while niox is 0.x, a minor
bump may break the wire protocol or config. The RPM spec's `%changelog`
and `debian/changelog` carry a one-line summary of each release.

## Unreleased

- **Lip sync on clients**: the "niox → host" output now reports the
  delay its audio picks up on the way to the host, so video players
  (mpv, VLC, Firefox, Chromium, and others that ask the sound server)
  hold video back to match, instead of showing it about 200ms before
  the sound. It starts at the latency target and follows the measured
  latency once the host reports it, including the host's own output
  path (its sound server's buffering and the output device, which is
  tens of milliseconds, and more on Bluetooth).

## 0.2.0 — 2026-11-27

Audio: clients' system audio plays on the host.

- **niox-session**, a new per-user service (`systemctl --user`) in its
  own package, `niox-session`, so keyboard/mouse-only installs don't
  need libroc or libpipewire. It's enabled for every user on install and
  starts for members of the `niox` group. It needs PipeWire as the
  user's sound server (with WirePlumber), and depends on it, so on a
  machine running PulseAudio, installing it switches that to PipeWire.
- **Audio from clients to the host**: on a client, a "niox → host"
  output becomes the default while the host is connected, so it works
  with no speakers attached. Its audio is encoded with Roc (with
  forward error correction) and carried over nioxd's encrypted session
  with the host, which plays it to its default output. If niox-session
  stops or crashes, the client's audio goes back to its own output.
- **Mixing**: a host plays every client at once (`always-mix`), or only
  the one with keyboard/mouse focus (`follow-focus`).
- **Config**: an `[audio]` table in `niox.toml` (`enabled`, `mode`,
  `latency_ms`), applied by reload like the rest.
- **Active seat**: with more than one user running niox-session, the
  user active at the machine's seat gets the audio.
- **Headless hosts** run niox-session under a lingering user in the
  `niox` and `audio` groups (see the README).

## 0.1.0 — 2026-11-26

First release: keyboard/mouse sharing between Linux machines on a LAN.

- **nioxd**, a system service that runs from boot, so it works at the
  login screen and on a host with nobody logged in. The host captures
  its keyboards and mice from evdev and forwards them to the focused
  client, which replays them through a uinput virtual device.
- **Encrypted transport**: Noise KK (X25519, ChaChaPoly, BLAKE2s) over a
  single UDP port (4777), with anti-replay, keepalives, and automatic
  re-handshake when either side restarts or the network drops.
- **Multiple clients**, with focus cycled by hotkey (NKMS's defaults,
  Left Meta + Grave. Held keys are released on focus change and disconnect, so
  nothing gets stuck down.
- **Hot-plug**: input devices added or removed mid-session are picked up
  without a restart.
- **Config** in `/etc/niox/niox.toml`, with peers paired by hand by
  public key. The keypair is generated on first start. `systemctl reload
  nioxd` or `nioxctl reload` applies hotkey and peer changes live.
- **nioxctl**: `status`, `pubkey`, `focus <target>`, `reload`, and `watch`
  over nioxd's control socket (`/run/niox/niox.sock`, group `niox`).
- **Packaging**: an RPM for Fedora (with a firewalld service) and a
  `.deb` for Debian, both built by `./build.sh`. Installing enables
  and starts nioxd, and upgrades keep the config and keypair.
