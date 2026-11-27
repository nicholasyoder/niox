# CLAUDE.md

niox: one host's keyboard/mouse (later audio, video) shared with LAN
clients at the evdev/uinput layer. Rust workspace, successor to NKMS and
audio-switcher. Read [ARCHITECTURE.md](ARCHITECTURE.md) for the design
and [ROADMAP.md](ROADMAP.md) for what's next. The first unchecked step
is the current one.

A whole-project code review is in progress, one subsystem per session:
[docs/review/PLAN.md](docs/review/PLAN.md) says what's left and how a
session works, and [docs/review/FINDINGS.md](docs/review/FINDINGS.md)
holds the findings waiting to be fixed.

## Checks

Every change leaves these passing:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Building the workspace needs `roc-toolkit-devel`, `pipewire-devel`, and
`clang-devel` (libroc for `niox-roc`, libpipewire for `niox-audio`,
libclang for their bindgen). The toolchain is pinned to Fedora's rustc (`rust-toolchain.toml`). Bump it
only together with Fedora's rust package. Workspace lints deny `unsafe` (opt
in per crate for FFI) and warn on `unwrap`, `print*`, `dbg!`, and `todo!`
outside tests.

## Working rules

- KM must never depend on a user session (it has to work at the login
  screen), and the input hot path stays in one process inside `nioxd`,
  with no IPC hop.
- Peers are configured by hand. There's no discovery, no external daemons,
  and no virtual NICs.
- Refer to roadmap steps by name, never by number. Check steps off in
  place in ROADMAP.md.
- [NKMS](https://github.com/nicholasyoder/nkms) is the reference for KM
  behavior. Don't port it line by line.
- Multi-machine checks run on the libvirt VMs in `tools/testvm/`
  (see its README). Boot only what a test needs, and shut the VMs down
  afterward. `tools/dev/deploy.sh` pushes dev builds to machines over SSH.
- Packaging: `./build.sh [rpm|deb]` writes to `target/dist/`. The version
  in `Cargo.toml` and the newest `debian/changelog` and `rpm/niox.spec`
  `%changelog` entries must match.

## Git

Two long-lived branches:

- **develop** is where all work happens: roadmap steps, bug fixes
  (including fixes to shipped behavior), and review fixes.
- **master** is the release line. Work reaches it only when a release is
  being cut: merge develop into master, then commit any fix-ups the
  release itself needs (version bumps, packaging) on master, and merge
  master back into develop afterward.

Don't force-push or rewrite published history.

Commits are one logical unit each, usually one roadmap step, with the
step's name as the subject. The body says what changed and why, grouped
by crate when the change touches several.
