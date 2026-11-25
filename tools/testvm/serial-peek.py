#!/usr/bin/env python3
"""Print a VM's serial console output for a few seconds (needs a pty for virsh console).
Usage: serial-peek.py <vm> [seconds]"""
import os, pty, re, select, sys, time
vm, secs = sys.argv[1], float(sys.argv[2]) if len(sys.argv) > 2 else 5
pid, fd = pty.fork()
if pid == 0:
    os.execvp("virsh", ["virsh", "-c", "qemu:///system", "console", vm, "--force"])
buf, end = b"", time.time() + secs
time.sleep(1); os.write(fd, b"\r")
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.5)
    if r:
        try: buf += os.read(fd, 65536)
        except OSError: break
os.write(fd, b"\x1d"); os.kill(pid, 9)
text = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", buf).decode(errors="replace")
print(text[-3000:])
