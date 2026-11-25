#!/usr/bin/env bash
# Builds nioxd, nioxctl, and niox-session (release) and installs them on each
# machine given, over SSH, with the packaged units (dist/nioxd.service and
# dist/niox-session.service, pointed at /usr/local/bin and installed in
# /etc/systemd/system and /etc/systemd/user, so they override an installed
# package's), then restarts nioxd there. niox-session's user unit is
# installed but not enabled; enable it per user (`systemctl --user enable
# --now niox-session`), and it is restarted for users already running it. Also creates the `niox`
# group, whose members may use nioxctl. The config
# (/etc/niox/niox.toml) is left alone; nioxd creates one with a fresh
# keypair on first start.
#
#   tools/dev/deploy.sh [-i identity_file] user@host...
set -euo pipefail

ssh_opts=()
if [[ ${1:-} == -i ]]; then
    ssh_opts=(-i "$2")
    shift 2
fi
if [[ $# -eq 0 ]]; then
    echo "usage: $0 [-i identity_file] user@host..." >&2
    exit 2
fi

root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -p nioxd -p nioxctl -p niox-session --manifest-path "$root/Cargo.toml"

for target in "$@"; do
    echo "==> $target"
    sed 's|/usr/bin/nioxd|/usr/local/bin/nioxd|' "$root/dist/nioxd.service" >"$root/target/nioxd.service"
    sed 's|/usr/bin/niox-session|/usr/local/bin/niox-session|' "$root/dist/niox-session.service" \
        >"$root/target/niox-session.service"
    scp "${ssh_opts[@]}" -q "$root/target/release/nioxd" "$root/target/release/nioxctl" \
        "$root/target/release/niox-session" "$root/target/nioxd.service" \
        "$root/target/niox-session.service" "$target:/tmp/"
    ssh "${ssh_opts[@]}" "$target" 'sudo sh -euc "
        getent group niox >/dev/null || groupadd --system niox
        install -m 0755 /tmp/nioxd /usr/local/bin/nioxd
        install -m 0755 /tmp/nioxctl /usr/local/bin/nioxctl
        install -m 0755 /tmp/niox-session /usr/local/bin/niox-session
        install -m 0644 /tmp/nioxd.service /etc/systemd/system/nioxd.service
        install -Dm 0644 /tmp/niox-session.service /etc/systemd/user/niox-session.service
        rm -f /tmp/nioxd /tmp/nioxctl /tmp/niox-session /tmp/nioxd.service /tmp/niox-session.service
        restorecon -F /usr/local/bin/nioxd /usr/local/bin/nioxctl /usr/local/bin/niox-session \
            /etc/systemd/system/nioxd.service /etc/systemd/user/niox-session.service 2>/dev/null || true
        systemctl daemon-reload
        systemctl enable nioxd
        systemctl restart nioxd
    "'
    # Pick up the new binary in user sessions already running it.
    ssh "${ssh_opts[@]}" "$target" 'systemctl --user daemon-reload 2>/dev/null &&
        systemctl --user try-restart niox-session 2>/dev/null || true'
done
