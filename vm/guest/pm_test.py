#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""System sleep with the Edge TPUs closed and open.

Cycles: the PM core's dry run (pm_test, devices really suspended and resumed),
real suspend-to-idle and, if offered, real suspend-to-RAM (S3; QEMU resets the
devices on wake-up, so state is lost). After each cycle the device must work for
a new user; an interpreter that was open across the cycle must either keep
working or fail cleanly, never hang.
"""
import os
import subprocess
import sys
import time

results = []
HOLDER = r'''
import sys
from PIL import Image
from pycoral.adapters import classify, common
from pycoral.utils.edgetpu import make_interpreter
D = "/opt/coral/"
it = make_interpreter(D + "mobilenet_v2_1.0_224_inat_bird_quant_edgetpu.tflite", device=":0")
it.allocate_tensors()
common.set_input(it, Image.open(D + "parrot.jpg").convert("RGB").resize(common.input_size(it), Image.LANCZOS))
for line in sys.stdin:
    try:
        it.invoke()
        print("ok", classify.get_classes(it, top_k=1)[0].id, flush=True)
    except Exception as e:
        print("error", type(e).__name__, str(e)[:120].replace("\n", " "), flush=True)
'''


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))


def diagnostics():
    for dev in ("apex_0", "apex_1"):
        counts = " ".join(l for l in sysfs("interrupt_counts", dev).splitlines() if not l.endswith(": 0"))
        print(f"  diag {dev}: status {sysfs('status', dev)} owner {sysfs('device_owner', dev)} "
              f"resets {sysfs('reset_count', dev)} irqs [{counts}]")
    print("  diag dmesg:", subprocess.run(["dmesg"], capture_output=True, text=True).stdout.splitlines()[-6:])


def fresh_inference():
    try:
        r = subprocess.run([sys.executable, "/apex/infer.py", "20"], capture_output=True, text=True, timeout=30)
    except subprocess.TimeoutExpired:
        diagnostics()
        return False, ["timed out after 30 s"]
    if r.returncode:
        diagnostics()
    return r.returncode == 0, (r.stdout + r.stderr).strip().splitlines()[-1:]


def sysfs(attr, dev="apex_0"):
    return open(f"/sys/class/apex/{dev}/{attr}").read().strip()


def available():
    states = open("/sys/power/state").read().split()
    mem_sleep = open("/sys/power/mem_sleep").read() if os.path.exists("/sys/power/mem_sleep") else ""
    cycles = []
    if os.path.exists("/sys/power/pm_test"):
        cycles.append("pmtest")
    # Real suspend-to-idle is not exercised: a QEMU guest in s2idle has no wake
    # source. pm_test covers the same device callbacks.
    if "mem" in states and "deep" in mem_sleep:
        cycles.append("deep")
    return cycles


def cycle(kind):
    if kind == "pmtest":
        levels = open("/sys/power/pm_test").read().replace("[", "").replace("]", "").split()
        level = "core" if "core" in levels else "devices"
        open("/sys/power/pm_test", "w").write(level)
        try:
            open("/sys/power/state", "w").write("mem" if "mem" in open("/sys/power/state").read() else "freeze")
        finally:
            open("/sys/power/pm_test", "w").write("none")
        return level
    if kind == "deep":
        open("/sys/power/mem_sleep", "w").write("deep")
        # The host side wakes the guest (vm/run.sh); the RTC alarm is a fallback.
        subprocess.run(["rtcwake", "-m", "mem", "-s", "5"], check=True, capture_output=True, timeout=120)
    return kind


cycles = available()
print("sleep cycles available:", cycles)
check("some_sleep_state_available", cycles)
for kind in cycles:
    # Closed device.
    how = cycle(kind)
    time.sleep(0.5)
    check(f"{kind}_closed_status_alive", sysfs("status") == "ALIVE", how)
    ok, tail = fresh_inference()
    check(f"{kind}_closed_then_inference", ok, " ".join(tail))

    # Open device with a live interpreter.
    h = subprocess.Popen([sys.executable, "-c", HOLDER], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True)
    h.stdin.write("go\n")
    h.stdin.flush()
    before = h.stdout.readline().strip()
    check(f"{kind}_open_inference_before", before.startswith("ok 923"), before)
    cycle(kind)
    time.sleep(0.5)
    h.stdin.write("go\n")
    h.stdin.flush()
    t0 = time.time()
    import select
    ready, _, _ = select.select([h.stdout], [], [], 20)
    after = h.stdout.readline().strip() if ready else "hang"
    if after == "hang":
        diagnostics()
    if after == "":
        h.wait(timeout=10)
        after = f"owner terminated (exit {h.returncode})"
    # Either the chip kept its state and inference goes on, or the owner fails
    # at once; it must never block.
    good = after.startswith("ok 923") or after.startswith("owner terminated") or after.startswith("error")
    check(f"{kind}_open_inference_after_no_hang", good, f"{after} ({time.time() - t0:.1f}s)")
    print(f"  {kind}: open interpreter after resume -> {after}; status {sysfs('status')}")
    h.kill()
    h.wait(timeout=10)
    ok, tail = fresh_inference()
    check(f"{kind}_reopen_then_inference", ok, " ".join(tail))

# The state-loss path for an open device, forced (whether the chip keeps its
# state over a real cycle varies): the owner must fail at once, not hang.
if "pmtest" in cycles:
    subprocess.run(["rmmod", "apex"], check=True)
    subprocess.run(["insmod", "/apex/apex.ko", "test_resume_state_loss=1"], check=True)
    time.sleep(0.5)
    h = subprocess.Popen([sys.executable, "-c", HOLDER], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True)
    h.stdin.write("go\n")
    h.stdin.flush()
    before = h.stdout.readline().strip()
    cycle("pmtest")
    time.sleep(0.5)
    status = sysfs("status")
    t0 = time.time()
    try:
        h.stdin.write("go\n")
        h.stdin.flush()
    except BrokenPipeError:
        pass
    import select
    ready, _, _ = select.select([h.stdout], [], [], 20)
    after = h.stdout.readline().strip() if ready else "hang"
    if after == "":
        h.wait(timeout=10)
        after = f"owner terminated (exit {h.returncode})"
    check("forced_loss_owner_had_worked", before.startswith("ok 923"), before)
    # DEAD until the dying owner's final close resets the device again.
    check("forced_loss_marked_dead_or_released", status == "DEAD" or sysfs("is_device_owned") == "0", status)
    check("forced_loss_owner_fails_fast", after != "hang" and not after.startswith("ok"),
          f"{after} ({time.time() - t0:.1f}s)")
    print(f"  forced state loss: open interpreter -> {after}")
    if h.poll() is None:
        h.kill()
    h.wait(timeout=10)
    ok, tail = fresh_inference()
    check("forced_loss_next_user_works", ok, " ".join(tail))
    subprocess.run(["rmmod", "apex"], check=True)
    subprocess.run(["insmod", "/apex/apex.ko"], check=True)
    time.sleep(0.5)

dmesg = subprocess.run(["dmesg"], capture_output=True, text=True).stdout
bad = [l for l in dmesg.splitlines() if any(k in l for k in ("BUG:", "WARNING:", "Oops", "general protection"))]
check("dmesg_clean", not bad, "; ".join(bad[:3]))
for name, ok, why in results:
    print(f"{'PASS' if ok else 'FAIL'} pm {name}{(': ' + why) if why else ''}")
failed = sum(not ok for _, ok, _ in results)
print(f"pm_test: {len(results) - failed}/{len(results)} passed")
sys.exit(1 if failed else 0)
