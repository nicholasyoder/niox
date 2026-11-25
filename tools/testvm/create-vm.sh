#!/usr/bin/env bash
# Create a niox test VM via an unattended kickstart install.
# Usage: create-vm.sh <name> <workstation|kde> <mac> <ip> [extra virt-install args...]
set -euo pipefail
name=$1 desktop=$2 mac=$3 ip=$4; shift 4
here=$(cd "$(dirname "$0")" && pwd)
export LIBVIRT_DEFAULT_URI=qemu:///system
iso=/opt/isos/Fedora-Everything-netinst-x86_64-45-1.4.iso
key=${NIOX_TESTVM_KEY:-$HOME/.ssh/niox_testvm.pub}

case $desktop in
  workstation) env=workstation-product-environment ;;
  kde) env=kde-desktop-environment ;;
  *) echo "unknown desktop: $desktop" >&2; exit 1 ;;
esac

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
sed -e "s|@HOSTNAME@|$name|" -e "s|@ENV@|$env|" -e "s|@SSHKEY@|$(cat "$key")|" \
  "$here/ks.cfg.in" > "$work/ks.cfg"

# Fixed address on the default NAT network (idempotent).
if ! virsh net-dumpxml default | grep -q "name='$name'"; then
  virsh net-update default add ip-dhcp-host \
    "<host mac='$mac' name='$name' ip='$ip'/>" --live --config
fi

virt-install \
  --name "$name" \
  --osinfo fedora-unknown \
  --vcpus 2 --memory 4096 \
  --disk pool=default,size=30,format=qcow2,bus=virtio,discard=unmap \
  --network network=default,mac="$mac",model=virtio \
  --graphics vnc --video virtio \
  --channel unix,target.type=virtio,target.name=org.qemu.guest_agent.0 \
  --location "$iso" \
  --initrd-inject "$work/ks.cfg" \
  --extra-args "inst.ks=file:/ks.cfg console=tty0 console=ttyS0,115200" \
  --check path_in_use=off \
  --noautoconsole --wait -1 \
  "$@"
