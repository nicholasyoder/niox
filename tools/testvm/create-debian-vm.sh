#!/usr/bin/env bash
# Create the Debian test VM (for the .deb) from Debian's generic cloud
# image, configured by cloud-init: user dev/devpass with the test SSH key,
# passwordless sudo, and GDM so login-screen tests work like on Fedora.
#
# Usage: create-debian-vm.sh <debian-13-generic-amd64.qcow2> [name mac ip]
# Download the image (and check it against SHA512SUMS) from
# https://cloud.debian.org/images/cloud/trixie/latest/
set -euo pipefail
image=$1
name=${2:-niox-debian} mac=${3:-52:54:00:a1:0e:04} ip=${4:-192.168.122.204}
export LIBVIRT_DEFAULT_URI=qemu:///system
key=${NIOX_TESTVM_KEY:-$HOME/.ssh/niox_testvm.pub}

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
cat > "$work/user-data" <<EOF
#cloud-config
hostname: $name
users:
  - name: dev
    groups: [sudo]
    shell: /bin/bash
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    lock_passwd: false
    plain_text_passwd: devpass
    ssh_authorized_keys: ["$(cat "$key")"]
ssh_pwauth: true
package_update: true
packages: [qemu-guest-agent, gdm3, gnome-shell, gnome-session, evtest, libinput-tools, rsync]
runcmd:
  - [systemctl, set-default, graphical.target]
power_state: {mode: reboot, condition: true}
EOF

# Fixed address on the default NAT network (idempotent).
if ! virsh net-dumpxml default | grep -q "name='$name'"; then
  virsh net-update default add ip-dhcp-host \
    "<host mac='$mac' name='$name' ip='$ip'/>" --live --config
fi

# The image becomes the VM's disk, uploaded through libvirt so no root is needed.
virsh vol-create-as default "$name.qcow2" 30G --format qcow2 >/dev/null
qemu-img convert -O raw "$image" "$work/disk.raw"
truncate -s 30G "$work/disk.raw"
qemu-img convert -O qcow2 "$work/disk.raw" "$work/disk.qcow2"
rm "$work/disk.raw"
virsh vol-upload --pool default "$name.qcow2" "$work/disk.qcow2"
disk=$(virsh vol-path --pool default "$name.qcow2")

virt-install \
  --name "$name" \
  --osinfo debian13 \
  --vcpus 2 --memory 4096 \
  --disk path="$disk",bus=virtio,discard=unmap \
  --network network=default,mac="$mac",model=virtio \
  --graphics vnc --video virtio \
  --channel unix,target.type=virtio,target.name=org.qemu.guest_agent.0 \
  --import \
  --cloud-init user-data="$work/user-data" \
  --noautoconsole
echo "$name is installing packages via cloud-init and reboots when done;"
echo "then shut it down and snapshot it: virsh snapshot-create-as $name clean"
