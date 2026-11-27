# niox — Network I/O Exchange

niox shares one machine's keyboard and mouse with the other machines
on your LAN, and plays their audio on it. You sit at the **host**, press
a hotkey to move input focus to a **client**, and the clients' audio
plays on the host's speakers. Video is planned later.

It works at the evdev/uinput layer rather than inside the desktop, so
it doesn't care about X11 vs. Wayland or which desktop a machine runs,
and keyboard/mouse work at the login screen before anyone has logged
in. Traffic is encrypted (Noise), peers are configured by hand by
address and public key, and there's no discovery, cloud service, or
virtual network interface.

niox replaces [NKMS](https://github.com/nicholasyoder/nkms)
(keyboard/mouse switching) and an earlier audio-switcher project.

**Status:** keyboard/mouse (multiple clients, device hot-plug, login
screen) and audio (each client's output mixed on the host, optionally
following focus) are done, packaged as RPMs and
`.deb`s. Next up is a tray icon and settings window; until then, setup
is a TOML file and `nioxctl`. Linux only: Fedora is the primary target,
Debian secondary, and macOS is deferred. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the design and
[ROADMAP.md](ROADMAP.md) for progress.

## Layout

A Cargo workspace; crates are added as the roadmap reaches them.

- `crates/nioxd` -- privileged system service: config owner, KM capture/send/receive, virtual input
- `crates/niox-session` -- optional user-session binary: audio, tray icon
- `crates/nioxctl` -- CLI over the nioxd control socket
- `crates/niox-km`, `crates/niox-audio` -- per-subsystem logic
- `crates/niox-roc` -- bindings to libroc (Roc's audio encoder/decoder)
- `crates/niox-transport` -- encrypted UDP transport shared by subsystems
- `crates/niox-ipc` -- local control channel (nioxd listens; niox-session and nioxctl connect)
- `crates/niox-settings` -- egui settings window (planned)
- `crates/niox-config` -- shared TOML config

## Development

`cargo test` across the workspace needs libroc and libclang, for the
audio crates: `sudo dnf install roc-toolkit-devel pipewire-devel clang-devel` (Debian:
`libroc-dev libpipewire-0.3-dev libclang-dev`).

## Building and installing

`./build.sh` builds the RPMs (with rpmbuild) and `.deb`s (in a Debian
container, with podman) into `target/dist/`; `./build.sh rpm` or
`./build.sh deb` builds just one. Installing the `niox` package enables and
starts `nioxd`, which writes `/etc/niox/niox.toml` with a fresh keypair.
Then, on each machine:

1. `sudo usermod -aG niox $USER` (log in again) to use `nioxctl` without
   sudo, and `nioxctl pubkey` to get this machine's public key.
2. Add the other machines as `[[peer]]` entries in `/etc/niox/niox.toml`
   (see ARCHITECTURE.md's Config section): on the host, each client with
   `role = "client"`; on a client, the host with `role = "host"`.
3. `sudo systemctl reload nioxd` (or `nioxctl reload`), then
   `nioxctl status`.
4. Fedora: `sudo firewall-cmd --permanent --add-service=niox && sudo firewall-cmd --reload`
   to open UDP 4777.

### Audio

Audio is the separate `niox-session` package (`sudo dnf install
niox-session-*.rpm`, or `sudo apt install ./niox-session_*.deb`), so
keyboard/mouse-only installs don't need libroc or libpipewire. Install
it on the host and on each client whose audio should play on the host.
It needs PipeWire as the sound server and depends on it (with
WirePlumber and PipeWire's PulseAudio replacement), so on a machine still
running PulseAudio, installing it switches that machine to PipeWire.
Its user unit is enabled for every user on install and starts at login
for members of the `niox` group (step 1 above). On a client, it makes a
"niox → host" output the default while the host is connected; on the
host, each client's audio plays to the default output. `[audio]` in
`niox.toml` turns it off or switches it to follow keyboard/mouse focus
(see ARCHITECTURE.md's Config section).

A headless host, where nobody logs in, needs a user to run it:

    sudo useradd -m niox-audio          # or any existing user
    sudo usermod -aG niox,audio niox-audio
    sudo loginctl enable-linger niox-audio

Lingering starts that user's service manager (and PipeWire) at boot,
without a login. The `audio` group gives it the sound card, which
logind otherwise only gives the user logged in at the seat. If someone
does log in at the host with niox-session running, the audio follows
the active user.

## License

niox is licensed under the [Mozilla Public License 2.0](LICENSE).
