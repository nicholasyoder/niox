#!/usr/bin/env bash
# Save a PNG screenshot of a VM's display. Usage: screenshot.sh <vm> <out.png>
set -euo pipefail
tmp=$(mktemp --suffix=.ppm); trap 'rm -f "$tmp"' EXIT
virsh -c qemu:///system screenshot "$1" "$tmp" >/dev/null
magick "$tmp" "$2"
