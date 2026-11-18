#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Concurrency and soak.

A: one owner process, many threads issuing map/unmap on disjoint ranges while
   others churn eventfds, partition queries and sysfs reads.
B: both TPUs running inference in parallel processes for SOAK seconds while
   sysfs is read continuously and the thermal poll runs every 10 ms.
"""
import ctypes
import fcntl
import mmap
import os
import struct
import subprocess
import sys
import threading
import time

PAGE = 4096
SOAK = int(sys.argv[1]) if len(sys.argv) > 1 else 60
G = 0xDC
ioc = lambda d, n, s: (d << 30) | (s << 16) | (G << 8) | n  # noqa: E731
MAP, UNMAP, SET_EFD, CLR_EFD, PTSIZE = ioc(1, 8, 32), ioc(1, 9, 32), ioc(1, 1, 16), ioc(1, 2, 8), ioc(3, 6, 32)
libc = ctypes.CDLL(None, use_errno=True)
results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))


def sysfs(attr, dev="apex_0"):
    return open(f"/sys/class/apex/{dev}/{attr}").read().strip()


# --- A: threaded ioctls on one fd -------------------------------------------------
fd = os.open("/dev/apex_0", os.O_RDWR)
errors = []
stop = threading.Event()


def mapper(t):
    buf = mmap.mmap(-1, 8 * PAGE)
    buf.write(b"z" * 8 * PAGE)
    host = ctypes.addressof(ctypes.c_char.from_buffer(buf))
    dev = (t * 64) << 12
    try:
        for _ in range(3000):
            fcntl.ioctl(fd, MAP, bytearray(struct.pack("<4Q", 0, 8 * PAGE, host, dev)))
            fcntl.ioctl(fd, UNMAP, bytearray(struct.pack("<4Q", 0, 8 * PAGE, 0, dev)))
    except OSError as e:
        errors.append(f"mapper {t}: {e}")


def churner():
    efd = libc.eventfd(0, 0)
    try:
        while not stop.is_set():
            for irq in range(13):
                fcntl.ioctl(fd, SET_EFD, bytearray(struct.pack("<2Q", irq, efd)))
            for irq in range(13):
                fcntl.ioctl(fd, CLR_EFD, irq)
            fcntl.ioctl(fd, PTSIZE, bytearray(32))
    except OSError as e:
        errors.append(f"churner: {e}")
    os.close(efd)


def reader(dev):
    attrs = ["temp", "status", "interrupt_counts", "node_0_num_mapped_pages", "throttle", "unique_id", "device_owner"]
    try:
        while not stop.is_set():
            for a in attrs:
                sysfs(a, dev)
    except OSError as e:
        errors.append(f"reader: {e}")


threads = [threading.Thread(target=mapper, args=(t,)) for t in range(8)]
aux = [threading.Thread(target=churner), threading.Thread(target=reader, args=("apex_0",))]
t0 = time.time()
for t in threads + aux:
    t.start()
for t in threads:
    t.join()
stop.set()
for t in aux:
    t.join()
check("threaded_map_unmap_no_errors", not errors, "; ".join(errors[:3]))
check("threaded_map_unmap_leaves_nothing_mapped", int(sysfs("node_0_num_mapped_pages")) == 0)
print(f"A: 8x3000 map/unmap pairs with eventfd churn and sysfs reads in {time.time() - t0:.1f}s")
os.close(fd)

# --- B: parallel inference soak ----------------------------------------------------
SOAKER = r'''
import sys, time
from PIL import Image
from pycoral.adapters import classify, common
from pycoral.utils.edgetpu import make_interpreter
D = "/opt/coral/"
it = make_interpreter(D + "mobilenet_v2_1.0_224_inat_bird_quant_edgetpu.tflite", device=sys.argv[1])
it.allocate_tensors()
common.set_input(it, Image.open(D + "parrot.jpg").convert("RGB").resize(common.input_size(it), Image.LANCZOS))
end, n, wrong = time.time() + float(sys.argv[2]), 0, 0
while time.time() < end:
    it.invoke()
    top = classify.get_classes(it, top_k=1)[0]
    wrong += top.id != 923
    n += 1
print(n, wrong)
'''
for a in ("temp_poll_interval",):
    for dev in ("apex_0", "apex_1"):
        open(f"/sys/class/apex/{dev}/{a}", "w").write("10")
stop.clear()
readers = [threading.Thread(target=reader, args=(d,)) for d in ("apex_0", "apex_1")]
for r in readers:
    r.start()
procs = [subprocess.Popen([sys.executable, "-c", SOAKER, f":{i}", str(SOAK)], stdout=subprocess.PIPE, text=True)
         for i in (0, 1)]
outs = [p.communicate(timeout=SOAK + 120)[0].split() for p in procs]
stop.set()
for r in readers:
    r.join()
for dev in ("apex_0", "apex_1"):
    open(f"/sys/class/apex/{dev}/temp_poll_interval", "w").write("5000")
for i, (p, out) in enumerate(zip(procs, outs)):
    n, wrong = (int(out[0]), int(out[1])) if len(out) == 2 else (0, -1)
    check(f"soak_tpu{i}_all_correct", p.returncode == 0 and n > 0 and wrong == 0, f"{n} invokes, {wrong} wrong")
    print(f"B: TPU :{i}: {n} invokes in {SOAK}s ({n / SOAK:.0f}/s), {wrong} misclassified")
check("soak_sysfs_reads_no_errors", not errors, "; ".join(errors[:3]))
for d in ("apex_0", "apex_1"):
    print(f"   {d}: temp {sysfs('temp', d)} m°C, {sysfs('throttle', d)}")

for name, ok, why in results:
    print(f"{'PASS' if ok else 'FAIL'} stress {name}{(': ' + why) if why else ''}")
failed = sum(not ok for _, ok, _ in results)
print(f"stress_test: {len(results) - failed}/{len(results)} passed")
sys.exit(1 if failed else 0)
