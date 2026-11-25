#!/usr/bin/env bash
# Builds nioxd and nioxctl (release) and installs them with the development
# unit on each machine given, over SSH, then restarts nioxd there. Also
# creates the `niox` group, whose members may use nioxctl. The config
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
    scp "${ssh_opts[@]}" -q "$root/target/release/nioxd" "$root/target/release/nioxctl" \
        "$root/tools/dev/nioxd.service" "$target:/tmp/"
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
