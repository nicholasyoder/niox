# niox test VMs

Three libvirt VMs (`qemu:///system`, visible in Cockpit) for multi-machine
niox testing. Built unattended from the Fedora 45 netinst ISO via kickstart.

| VM              | IP              | Desktop             | Role in tests                       |
|-----------------|-----------------|---------------------|-------------------------------------|
| `niox-host`     | 192.168.122.201 | Fedora Workstation  | KM host (has an extra virtio relative mouse) |
| `niox-client-a` | 192.168.122.202 | Fedora Workstation  | client, GDM login-screen tests      |
| `niox-client-b` | 192.168.122.203 | Fedora KDE Plasma   | client, Plasma Login (plasmalogin) / KWin |

All: 2 vCPU, 4 GiB RAM, 30 GB qcow2, default NAT network (fixed DHCP
reservations), qemu-guest-agent, UDP 4777 open in firewalld.

VMs are kept shut off; boot them (`virsh start <vm>`) only for a test run
and shut them down after (`virsh shutdown <vm>`).

Login: user `dev`, password `devpass`, passwordless sudo. SSH with the
dedicated key: `ssh -i ~/.ssh/niox_testvm dev@192.168.122.201`.

## Rebuilding

    ./create-vm.sh niox-host     workstation 52:54:00:a1:0e:01 192.168.122.201 --input mouse,bus=virtio
    ./create-vm.sh niox-client-a workstation 52:54:00:a1:0e:02 192.168.122.202
    ./create-vm.sh niox-client-b kde         52:54:00:a1:0e:03 192.168.122.203

Each takes 15-30 minutes (packages come from the Fedora mirrors). Then
snapshot a clean state: `virsh snapshot-create-as <vm> clean`.

## Driving tests

All verified working:

- Key input on the host VM lands on its `AT Translated Set 2 keyboard`
  evdev device: `virsh send-key niox-host KEY_LEFTMETA KEY_GRAVE`.
- Mouse input lands as `REL_X`/`REL_Y`/`BTN_*` on the `QEMU Virtio Mouse`:
  `virsh qemu-monitor-command niox-host --hmp 'mouse_move 40 -20'`,
  `... --hmp 'mouse_button 1'` (bitmask; `0` releases).
- Observe a client: `sudo evtest /dev/input/eventN` or
  `sudo libinput debug-events` over SSH.
- See the screen (e.g. the login prompt): `./screenshot.sh niox-client-a out.png`.
- Serial console (e.g. a stuck kickstart): `python3 -I serial-peek.py <vm> [secs]`.
- Reset: `virsh snapshot-revert <vm> clean` (snapshot taken shut off,
  right after install; then `virsh start <vm>`).

Set `LIBVIRT_DEFAULT_URI=qemu:///system` (or pass `-c`) so `virsh` sees
the same VMs as Cockpit.

Notes:
- While niox grabs input on `niox-host`, typing into its Cockpit console
  is forwarded to the active client, by design.
- The netinst install sometimes lands on a very slow mirror (tens of
  KiB/s). If an install's network counter (`virsh domifstat`) stalls,
  destroy/undefine it and rerun; the mirrorlist picks another mirror.
