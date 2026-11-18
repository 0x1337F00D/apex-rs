#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Unbind the driver while a process holds the device open with live mappings,
then rebind it and reload the module. Checks that user space sees ENODEV/SIGBUS
instead of the kernel touching freed state, and that everything comes back."""
import ctypes
import errno
import fcntl
import mmap
import os
import signal
import struct
import subprocess
import sys
import time

PAGE = 4096
DEV, NAME = "/dev/apex_0", "apex_0"
pci = os.path.basename(os.readlink(f"/sys/class/apex/{NAME}/device"))
drv = "/sys/bus/pci/drivers/apex"
G = 0xDC
MAP_BUFFER = (1 << 30) | (32 << 16) | (G << 8) | 8
CONFIG_COHERENT = (3 << 30) | (32 << 16) | (G << 8) | 11
RESET = (G << 8) | 0
results = []


def check(name, cond, detail=""):
    results.append((name, bool(cond), detail))


def addr_of(m):
    return ctypes.addressof(ctypes.c_char.from_buffer(m))


r, w = os.pipe()
holder = os.fork()
if holder == 0:
    os.close(r)
    fd = os.open(DEV, os.O_RDWR)
    regs = mmap.mmap(fd, PAGE, flags=mmap.MAP_SHARED, offset=0x48000)
    fcntl.ioctl(fd, CONFIG_COHERENT, bytearray(struct.pack("<4Q", 0, 1, 0x4000, 0)))
    coh = mmap.mmap(fd, 0x4000, flags=mmap.MAP_SHARED, offset=0x1000000)
    buf = mmap.mmap(-1, 16 * PAGE)
    buf.write(b"x" * 16 * PAGE)
    fcntl.ioctl(fd, MAP_BUFFER, bytearray(struct.pack("<4Q", 0, 16 * PAGE, addr_of(buf), 0)))
    os.write(w, b"r")
    signal.pause()  # parent unbinds, then signals us
    os._exit(0)

os.close(w)
os.read(r, 1)
check("held_mapping_counted", int(open(f"/sys/class/apex/{NAME}/node_0_num_mapped_pages").read()) == 16)

with open(f"{drv}/unbind", "w") as f:
    f.write(pci)
check("node_removed_on_unbind", not os.path.exists(DEV))
check("class_entry_removed_on_unbind", not os.path.exists(f"/sys/class/apex/{NAME}"))

# The holder releases its files (and the coherent VMA) only now, after unbind.
os.kill(holder, signal.SIGUSR1)  # default action terminates; closing its files
_, st = os.waitpid(holder, 0)
check("holder_exits_cleanly_after_unbind", os.WIFSIGNALED(st) and os.WTERMSIG(st) == signal.SIGUSR1)

with open(f"{drv}/bind", "w") as f:
    f.write(pci)
for _ in range(100):
    if os.path.exists(DEV):
        break
    time.sleep(0.05)
check("rebind_recreates_node", os.path.exists(DEV))

# Stale fd + mappings in a process that survives the unbind. The process that
# owns the mapping must touch it itself (the VMAs are VM_DONTCOPY, so a forked
# helper would not even have them): it reports the ioctl result, then accesses
# the mapping and should die of SIGBUS.
def stale_access(which):
    r, w = os.pipe()
    r2, w2 = os.pipe()
    child = os.fork()
    if child == 0:
        os.close(r)
        os.close(w2)
        fd = os.open(DEV, os.O_RDWR)
        regs = mmap.mmap(fd, PAGE, flags=mmap.MAP_SHARED, offset=0x48000)
        fcntl.ioctl(fd, CONFIG_COHERENT, bytearray(struct.pack("<4Q", 0, 1, 0x4000, 0)))
        coh = mmap.mmap(fd, 0x4000, flags=mmap.MAP_SHARED, offset=0x1000000)
        os.write(w, b"r")
        os.read(r2, 1)  # wait for unbind
        try:
            fcntl.ioctl(fd, RESET)
            err = 0
        except OSError as e:
            err = e.errno
        os.write(w, struct.pack("<i", err))
        m, off = (regs, 0x6E0) if which == "register" else (coh, 0)
        ctypes.c_uint64.from_address(addr_of(m) + off).value  # must fault
        os._exit(0)
    os.close(w)
    os.close(r2)
    os.read(r, 1)
    with open(f"{drv}/unbind", "w") as f:
        f.write(pci)
    os.write(w2, b"g")
    (err,) = struct.unpack("<i", os.read(r, 4))
    _, st = os.waitpid(child, 0)
    sig = os.WTERMSIG(st) if os.WIFSIGNALED(st) else 0
    check(f"stale_fd_ioctl_enodev_{which}", err == errno.ENODEV, errno.errorcode.get(err, err))
    check(f"stale_{which}_mapping_sigbus", sig == signal.SIGBUS, f"signal {sig}")
    with open(f"{drv}/bind", "w") as f:
        f.write(pci)
    for _ in range(100):
        if os.path.exists(DEV):
            break
        time.sleep(0.05)
    check(f"rebind_after_{which}", os.path.exists(DEV))


stale_access("register")
stale_access("coherent")

# Module reload with no users.
ko = sys.argv[1] if len(sys.argv) > 1 else "/apex/apex.ko"
for i in range(3):
    rm = subprocess.run(["rmmod", "apex"], capture_output=True, text=True)
    ins = subprocess.run(["insmod", ko], capture_output=True, text=True)
    time.sleep(0.5)
    check(f"reload_{i}", rm.returncode == 0 and ins.returncode == 0 and os.path.exists(DEV),
          (rm.stderr + ins.stderr).strip())
dmesg = subprocess.run(["dmesg"], capture_output=True, text=True).stdout
bad = [l for l in dmesg.splitlines() if any(k in l for k in ("BUG:", "WARNING:", "Oops", "general protection", "refcount_t:", "KASAN:", "list_del corruption", "possible circular locking", "DMA-API: apex"))]
check("dmesg_clean", not bad, "; ".join(bad[:3]))

for name, ok, why in results:
    print(f"{'PASS' if ok else 'FAIL'} lifecycle {name}{(': ' + why) if why else ''}")
failed = sum(not ok for _, ok, _ in results)
print(f"lifecycle_test: {len(results) - failed}/{len(results)} passed")
sys.exit(1 if failed else 0)
