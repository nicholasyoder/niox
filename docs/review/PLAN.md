# Code review plan

A whole-project review, after a long run of features landed without one.
It looks for architectural problems, bugs, logic repeated across crates,
and latency or performance costs, with the KM hot path first.

The workspace is about 18k lines of Rust in 9 crates (about 14k without
tests and examples), which is too much to review carefully in one
session. It's split by subsystem below, roughly 3–5k lines a session,
ordered by risk: KM latency and correctness, then the transport's
security, then the control plane, audio, and the rest.

## How each session works

- Read every non-test line of the session's scope, and skim its tests to
  see what is and isn't covered.
- **Small fixes** (a few lines, obviously right, covered by a test where
  one makes sense) are committed to `develop` as the review goes, one
  logical fix per commit, with the checks in CLAUDE.md passing.
- **Anything larger** goes in [FINDINGS.md](FINDINGS.md) under the
  session's heading: what's wrong, where, a failure scenario, a severity,
  and a suggested fix. Don't fix these during the review.
- Tick the session off below, with the commits it made.
- Last, triage FINDINGS.md into ROADMAP.md: bugs go under *Bugfixes*,
  bigger items become steps.

Severity: **high** means wrong behavior users hit, or a security hole.
**medium** means a real bug in a rare case, or a measurable latency cost.
**low** means cleanup, a small inefficiency, or hardening.

## Sessions

- [x] **KM** (about 5.7k lines): `niox-km` (wire, caps, hotkey, focus,
  held, host, client, device/*), `nioxd/src/km/*`, `nioxd/src/main.rs`.
  Focus: hotkey and held-key correctness, stuck keys, lock ordering,
  per-frame cost on the capture → send and receive → uinput paths,
  hot-plug.
  Commits: `a1e3899`, `859c236`, `d06665e`, `94ff494`.
- [x] **Transport** (about 4k lines): `niox-transport` (packet, handshake,
  replay, session, endpoint, driver, resolve) and its tests.
  Focus:
  - Noise KK use and the init-timestamp replay check.
  - Index allocation, and the replay window's edges.
  - The cost of an unauthenticated init: a DH per datagram from any
    source, so CPU DoS.
  - Address roaming: can a spoofed source redirect replies?
  - Sessions never rekey, so one session lives as long as the link
    (WireGuard rekeys every 2 minutes).
  - Locks and allocations in `seal_and_send` (the KM send path), and
    driver-thread blocking.
  - Liveness timer edges, and peer add, remove, and update races.
  Commits: `84751fa`, `54a3760`.
- [x] **Control plane** (about 4.5k lines): `niox-ipc` (protocol, server,
  publisher, client, arbiter, fds, seat, audio), `niox-config` (load,
  write, validate, raw, address, key), `nioxctl`, `nioxd/src/audio.rs`.
  Focus:
  - Every request is untrusted input from any `niox` group member:
    line-length and JSON limits, slow and stuck clients, fd passing.
  - Socket setup races: the bind, chmod, and rename sequence.
  - Atomic config writes and file modes.
  - The audio channel arbiter (seat0 polling) and the relay's
    non-blocking forwarding.
  - Status and enum mapping repeated between crates.
  Commits: `84ed881`, `f821fd2`, `1632ba8`.
- [ ] **Audio** (about 4.5k lines): `niox-roc` (all of the workspace's
  `unsafe`), `niox-audio` (engine, streams, lib), `niox-session` (main,
  view, audio).
  Focus:
  - FFI soundness: lifetimes, thread-safety claims (`Send`/`Sync`
    impls), and buffer sizes.
  - Blocking or allocating on the PipeWire thread.
  - Reconnect and backoff logic.
  - Default-sink restore.
  - State duplicated between the view and the engine.
- [ ] **Cross-cutting** (small): `build.sh`, `rpm/`, `debian/`,
  `dist/*.service`, `tools/`, and the examples.
  Focus:
  - Unit hardening against what each binary needs.
  - Docs drift (ARCHITECTURE.md against the code).
  - Duplicate dependency versions (`cargo tree -d`) and advisories
    (`cargo audit`, if available).
  - Test gaps found in earlier sessions.
  - Then triage FINDINGS.md into ROADMAP.md.
