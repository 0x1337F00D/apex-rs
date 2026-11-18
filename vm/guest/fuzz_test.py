#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Random ioctls and mmaps against /dev/apex_0. Success criterion: the kernel
survives (no oops/warning) and the device still works afterwards."""
import ctypes
import errno
import fcntl
import mmap
import os
import random
import struct
import subprocess
import sys

ITER = int(sys.argv[1]) if len(sys.argv) > 1 else 50000
rng = random.Random(0xA9E)
libc = ctypes.CDLL(None, use_errno=True)
libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]
PAGE = 4096
own = mmap.mmap(-1, 64 * PAGE)
own.write(b"f" * 64 * PAGE)
own_addr = ctypes.addressof(ctypes.c_char.from_buffer(own))
arena = ctypes.create_string_buffer(4096)
seen = {}


def rand_u64():
    return rng.choice([0, 1, PAGE, own_addr, own_addr + PAGE * rng.randrange(64), 1 << 63,
                       (1 << 63) | (rng.randrange(8192) << 21), rng.getrandbits(64), 0xFFFFFFFFFFFFFFFF,
                       rng.randrange(1 << 24) << 12, 0x1000000, 64 * PAGE, 3 << 1, 2 << 1])


fd = os.open("/dev/apex_0", os.O_RDWR)
cmds = []
for nr in range(16):
    for d in (0, 1, 2, 3):
        for size in (0, 4, 8, 16, 24, 32, 40):
            cmds.append((d << 30) | (size << 16) | (0xDC << 8) | nr)
            cmds.append((d << 30) | (size << 16) | (0x7F << 8) | nr)
def ioc(d, t, n, size):
    return (d << 30) | (size << 16) | (t << 8) | n


# The real commands (as libedgetpu encodes them), chosen most of the time so the
# fuzzer reaches argument validation rather than the unknown-command path.
REAL = [ioc(0, 0xDC, 0, 0), ioc(1, 0xDC, 1, 16), ioc(1, 0xDC, 2, 8), ioc(2, 0xDC, 4, 8),
        ioc(3, 0xDC, 5, 32), ioc(3, 0xDC, 6, 32), ioc(1, 0xDC, 7, 32), ioc(1, 0xDC, 8, 32),
        ioc(1, 0xDC, 9, 32), ioc(0, 0xDC, 10, 0), ioc(3, 0xDC, 11, 32), ioc(1, 0xDC, 12, 40),
        ioc(1, 0xDC, 13, 32), ioc(1, 0x7F, 0, 16), ioc(1, 0x7F, 1, 4)]
for i in range(ITER):
    cmd = rng.choice(REAL) if rng.random() < 0.85 else rng.choice(cmds)
    words = [rand_u64() for _ in range(6)]
    data = struct.pack("<6Q", *words)
    ctypes.memmove(arena, data, len(data))
    if rng.random() < 0.15:
        arg = rng.choice([0, 1, 13, 12, 0xFFFFFFFFFFFFFFFF, own_addr - 8, 0x1000])  # value / bad pointer
    else:
        arg = ctypes.addressof(arena)
    r = libc.ioctl(fd, cmd, arg)
    e = ctypes.get_errno() if r < 0 else 0
    seen[e] = seen.get(e, 0) + 1
    if rng.random() < 0.05:
        off = rng.choice([0, 0x40000, 0x44000, 0x48000, 0x41000, 0x1000000, 0x1001000, rng.randrange(1 << 20) << 12])
        ln = rng.choice([PAGE, 2 * PAGE, 0x4000, 0x10000])
        flags = rng.choice([mmap.MAP_SHARED, mmap.MAP_PRIVATE])
        try:
            m = mmap.mmap(fd, ln, flags=flags, offset=off)
            m.close()
            seen["mmap-ok"] = seen.get("mmap-ok", 0) + 1
        except (OSError, ValueError):
            seen["mmap-err"] = seen.get("mmap-err", 0) + 1
    if rng.random() < 0.002:
        os.close(fd)  # final close cleans up whatever got mapped
        fd = os.open("/dev/apex_0", os.O_RDWR)
os.close(fd)
summary = ", ".join(f"{errno.errorcode.get(k, k) if isinstance(k, int) else k}:{v}" for k, v in sorted(seen.items(), key=str))
print(f"fuzz: {ITER} ioctls; outcomes {summary}")
mapped = int(open("/sys/class/apex/apex_0/node_0_num_mapped_pages").read())
owned = int(open("/sys/class/apex/apex_0/is_device_owned").read())
inf = subprocess.run([sys.executable, "/apex/infer.py", "20"], capture_output=True, text=True)
dmesg = subprocess.run(["dmesg"], capture_output=True, text=True).stdout
bad = [l for l in dmesg.splitlines() if any(k in l for k in ("BUG:", "WARNING:", "Oops", "general protection"))]
checks = [("state_released", mapped == 0 and owned == 0, f"mapped {mapped} owned {owned}"),
          ("inference_after_fuzz", inf.returncode == 0, inf.stdout.strip().splitlines()[-1:] if inf.stdout else inf.stderr[-200:]),
          ("dmesg_clean", not bad, "; ".join(bad[:3]))]
for name, ok, why in checks:
    print(f"{'PASS' if ok else 'FAIL'} fuzz {name}{(': ' + str(why)) if not ok else ''}")
print(f"fuzz_test: {sum(ok for _, ok, _ in checks)}/{len(checks)} passed")
sys.exit(0 if all(ok for _, ok, _ in checks) else 1)
