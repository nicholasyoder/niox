# niox — Network I/O Exchange

niox lets one host machine (with a physical keyboard, mouse, and
eventually screen) control and exchange input, audio, and eventually
video with other machines over the LAN.

It's the successor to two earlier projects kept under `old/` for
reference: `nkms` (keyboard/mouse switching) and `audio-switcher`
(network audio mixing).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design and
[ROADMAP.md](ROADMAP.md) for progress.

**Status:** early development. niox is being built in Rust, starting
with keyboard/mouse. The goal is full Linux parity with NKMS
(multi-client, hot-plug, login-screen support, RPM/deb packaging) so NKMS
can be retired. Audio and the web UI come after that. An earlier Go
prototype that reached single-client keyboard/mouse is archived in
`archive/go/` for reference.

## Layout

A Cargo workspace; crates are added as the roadmap reaches them.

- `crates/nioxd` -- privileged system service: config owner, KM capture/send/receive, virtual input
- `crates/niox-session` -- optional user-session binary: audio, control API, web UI
- `crates/nioxctl` -- CLI over the nioxd control socket
- `crates/niox-km`, `crates/niox-audio` -- per-subsystem logic
- `crates/niox-transport` -- encrypted UDP transport shared by subsystems
- `crates/niox-ipc` -- local control channel (nioxd listens; niox-session and nioxctl connect)
- `crates/niox-control` -- HTTP API + web UI
- `crates/niox-config` -- shared TOML config
- `web/` -- control UI frontend source
- `archive/go/` -- archived Go prototype, reference only

## Building and installing

`./build.sh` builds an RPM (with rpmbuild) and a `.deb` (in a Debian
container, with podman) into `target/dist/`; `./build.sh rpm` or
`./build.sh deb` builds just one. Installing either one enables and
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

## License

niox is licensed under the [Mozilla Public License 2.0](LICENSE).
