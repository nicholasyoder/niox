# Changelog

Notable changes to niox, newest first. Versions follow
[Semantic Versioning](https://semver.org/); while niox is 0.x, a minor
bump may break the wire protocol or config. The RPM spec's `%changelog`
and `debian/changelog` carry a one-line summary of each release.

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
