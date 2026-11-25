#!/usr/bin/env bash
# Builds nioxd and nioxctl (release) and installs them on each machine given,
# over SSH, with the packaged unit (dist/nioxd.service, pointed at
# /usr/local/bin and installed in /etc/systemd/system, so it overrides an
# installed package's), then restarts nioxd there. Also creates the `niox`
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
cargo build --release -p nioxd -p nioxctl --manifest-path "$root/Cargo.toml"

for target in "$@"; do
    echo "==> $target"
    sed 's|/usr/bin/nioxd|/usr/local/bin/nioxd|' "$root/dist/nioxd.service" >"$root/target/nioxd.service"
    scp "${ssh_opts[@]}" -q "$root/target/release/nioxd" "$root/target/release/nioxctl" \
        "$root/target/nioxd.service" "$target:/tmp/"
    ssh "${ssh_opts[@]}" "$target" 'sudo sh -euc "
        getent group niox >/dev/null || groupadd --system niox
        install -m 0755 /tmp/nioxd /usr/local/bin/nioxd
        install -m 0755 /tmp/nioxctl /usr/local/bin/nioxctl
        install -m 0644 /tmp/nioxd.service /etc/systemd/system/nioxd.service
        rm -f /tmp/nioxd /tmp/nioxctl /tmp/nioxd.service
        restorecon -F /usr/local/bin/nioxd /usr/local/bin/nioxctl /etc/systemd/system/nioxd.service 2>/dev/null || true
        systemctl daemon-reload
        systemctl enable nioxd
        systemctl restart nioxd
    "'
done
