#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""ABI tests for /dev/apex_N, run inside the passthrough VM on real hardware.

Exercises every ioctl libedgetpu uses, the mmap layout, ownership rules and the
malformed-input paths, using only the Python standard library.
"""
import ctypes
import errno
import fcntl
import mmap
import os
import signal
import struct
import sys
import time

PAGE = 4096
libc = ctypes.CDLL(None, use_errno=True)
libc.eventfd.argtypes = [ctypes.c_uint, ctypes.c_int]
libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]


def ioc(d, t, n, size):
    return (d << 30) | (size << 16) | (t << 8) | n


W, R = 1, 2
G, A = 0xDC, 0x7F
RESET = ioc(0, G, 0, 0)
SET_EVENTFD = ioc(W, G, 1, 16)
CLEAR_EVENTFD = ioc(W, G, 2, 8)
NUMBER_PAGE_TABLES = ioc(R, G, 4, 8)
PAGE_TABLE_SIZE = ioc(R | W, G, 5, 32)
SIMPLE_PAGE_TABLE_SIZE = ioc(R | W, G, 6, 32)
PARTITION = ioc(W, G, 7, 32)
MAP_BUFFER = ioc(W, G, 8, 32)
UNMAP_BUFFER = ioc(W, G, 9, 32)
CLEAR_INTERRUPT_COUNTS = ioc(0, G, 10, 0)
CONFIG_COHERENT = ioc(R | W, G, 11, 32)
MAP_BUFFER_FLAGS = ioc(W, G, 12, 40)
MAP_DMABUF = ioc(W, G, 13, 32)
GATE_CLOCK = ioc(W, A, 0, 16)
PERFORMANCE = ioc(W, A, 1, 4)
EXTENDED = 1 << 63
COHERENT_OFFSET = 0x1000000

DEV = sys.argv[1] if len(sys.argv) > 1 else "/dev/apex_0"
NAME = os.path.basename(DEV)
SYS = f"/sys/class/apex/{NAME}"

results = []


def sysfs(attr):
    with open(f"{SYS}/{attr}") as f:
        return f.read().strip()


def pt(index=0, size=0, host=0, dev=0):
    return bytearray(struct.pack("<4Q", index, size, host, dev))


def pt_flags(size, host, dev, flags):
    return bytearray(struct.pack("<4QII", 0, size, host, dev, flags, 0))


def coherent(enable, size, dma=0):
    return bytearray(struct.pack("<4Q", 0, enable, size, dma))


def expect_errno(code, fn, *args, **kwargs):
    try:
        fn(*args, **kwargs)
    except OSError as e:
        if e.errno != code:
            raise AssertionError(f"expected {errno.errorcode[code]}, got {errno.errorcode.get(e.errno, e.errno)}")
        return
    raise AssertionError(f"expected {errno.errorcode[code]}, call succeeded")


def addr_of(m):
    return ctypes.addressof(ctypes.c_char.from_buffer(m))


def anon(pages):
    m = mmap.mmap(-1, pages * PAGE, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
    m.write(b"\xa5" * (pages * PAGE))  # fault the pages in
    return m


def test(fn):
    name = fn.__name__
    try:
        fn()
    except Exception as e:  # noqa: BLE001 - report every failure
        import traceback
        line = traceback.extract_tb(e.__traceback__)[-1].lineno
        results.append((name, False, f"line {line}: {type(e).__name__}: {e}"))
    else:
        results.append((name, True, ""))
    return fn


def rw():
    return os.open(DEV, os.O_RDWR | os.O_CLOEXEC)


@test
def sysfs_attributes_are_sane():
    t = int(sysfs("temp"))
    assert 5_000 < t < 100_000, f"temperature {t} m°C"
    assert int(sysfs("node_0_page_table_entries")) == 8192
    assert sysfs("status") == "ALIVE"
    uid = sysfs("unique_id")
    assert len(uid) == 32 and int(uid, 16) != 0, uid
    assert int(sysfs("is_device_owned")) == 0
    lines = sysfs("interrupt_counts").splitlines()
    assert len(lines) == 13 and lines[0].startswith("0x00: ")
    trips = [int(sysfs(f"trip_point{i}_temp")) for i in range(3)]
    assert trips == sorted(trips) and abs(trips[0] - 85000) < 250, trips
    assert sysfs("framework_version") == "1.1.4"


@test
def readonly_open_allows_queries_only():
    fd = os.open(DEV, os.O_RDONLY)
    try:
        buf = pt()
        fcntl.ioctl(fd, PAGE_TABLE_SIZE, buf)
        assert struct.unpack_from("<Q", buf, 8)[0] == 8192
        n = bytearray(8)
        fcntl.ioctl(fd, NUMBER_PAGE_TABLES, n)
        assert struct.unpack("<Q", n)[0] == 1
        m = anon(1)
        expect_errno(errno.EPERM, fcntl.ioctl, fd, MAP_BUFFER, pt(size=PAGE, host=addr_of(m)))
        expect_errno(errno.EPERM, fcntl.ioctl, fd, RESET)
        assert int(sysfs("is_device_owned")) == 0
    finally:
        os.close(fd)


@test
def ownership_is_per_process():
    fd = rw()
    try:
        assert int(sysfs("is_device_owned")) == 1
        assert int(sysfs("device_owner")) == os.getpid()
        fd2 = rw()  # same process may open again
        assert int(sysfs("write_open_count")) == 2
        os.close(fd2)
        # Root (CAP_SYS_ADMIN) may open an owned device, as with gasket; test
        # an unprivileged second process.
        os.chmod(DEV, 0o666)
        pid = os.fork()
        if pid == 0:
            os.setgid(65534)
            os.setuid(65534)
            try:
                os.open(DEV, os.O_RDWR)
                os._exit(1)
            except OSError as e:
                os._exit(0 if e.errno == errno.EPERM else 2)
        _, status = os.waitpid(pid, 0)
        assert os.waitstatus_to_exitcode(status) == 0, "second process could open for writing"
    finally:
        os.chmod(DEV, 0o600)
        os.close(fd)
    assert int(sysfs("is_device_owned")) == 0
    assert int(sysfs("write_open_count")) == 0


@test
def page_table_partition_and_sizes():
    fd = rw()
    try:
        fcntl.ioctl(fd, PARTITION, pt(size=4096))
        buf = pt()
        fcntl.ioctl(fd, SIMPLE_PAGE_TABLE_SIZE, buf)
        assert struct.unpack_from("<Q", buf, 8)[0] == 4096
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, PARTITION, pt(size=8193))
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, PARTITION, pt(index=1, size=10))
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, PAGE_TABLE_SIZE, pt(index=1))
    finally:
        os.close(fd)


@test
def map_unmap_simple_and_extended():
    fd = rw()
    try:
        fcntl.ioctl(fd, PARTITION, pt(size=4096))
        m = anon(16)
        a = addr_of(m)
        fcntl.ioctl(fd, MAP_BUFFER, pt(size=16 * PAGE, host=a, dev=0x10000))
        assert int(sysfs("node_0_num_mapped_pages")) == 16
        # Overlap is refused, a different size cannot unmap it.
        expect_errno(errno.EBUSY, fcntl.ioctl, fd, MAP_BUFFER, pt(size=PAGE, host=a, dev=0x1f000))
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, UNMAP_BUFFER, pt(size=8 * PAGE, dev=0x10000))
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, UNMAP_BUFFER, pt(size=16 * PAGE, dev=0x11000))
        # Extended mapping spanning three subtables, write-only for the device.
        big = anon(1100)
        dev = EXTENDED | (5 << 21) | (500 << 12)
        fcntl.ioctl(fd, MAP_BUFFER_FLAGS, pt_flags(1100 * PAGE, addr_of(big), dev, 2 << 1))
        assert int(sysfs("node_0_num_mapped_pages")) == 16 + 1100
        fcntl.ioctl(fd, UNMAP_BUFFER, pt(size=1100 * PAGE, dev=dev))
        fcntl.ioctl(fd, UNMAP_BUFFER, pt(size=16 * PAGE, dev=0x10000))
        assert int(sysfs("node_0_num_mapped_pages")) == 0
        # Partitioning with live extended subtables is refused, then allowed.
        fcntl.ioctl(fd, MAP_BUFFER, pt(size=PAGE, host=a, dev=EXTENDED))
        expect_errno(errno.EBUSY, fcntl.ioctl, fd, PARTITION, pt(size=8192))
        fcntl.ioctl(fd, UNMAP_BUFFER, pt(size=PAGE, dev=EXTENDED))
        fcntl.ioctl(fd, PARTITION, pt(size=8192))
        del m, big
    finally:
        os.close(fd)


@test
def map_rejects_malformed_requests():
    fd = rw()
    try:
        m = anon(4)
        a = addr_of(m)
        cases = [
            (errno.EINVAL, pt(size=PAGE, host=a + 8, dev=0)),        # unaligned host
            (errno.EINVAL, pt(size=0, host=a, dev=0)),               # empty
            (errno.EINVAL, pt(size=100, host=a, dev=0)),             # partial page
            (errno.EINVAL, pt(size=PAGE, host=a, dev=8192 << 12)),   # past the table
            (errno.EINVAL, pt(size=1 << 62, host=a, dev=0)),         # absurd size
            (errno.EINVAL, pt(size=PAGE, host=a, dev=EXTENDED)),     # no extended region
            (errno.EINVAL, pt(index=1, size=PAGE, host=a, dev=0)),   # no such table
            (errno.EFAULT, pt(size=PAGE, host=0x1000, dev=0)),       # not mapped
        ]
        for code, req in cases:
            expect_errno(code, fcntl.ioctl, fd, MAP_BUFFER, req)
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_BUFFER_FLAGS, pt_flags(PAGE, a, 0, 3 << 1))  # DMA_NONE
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_BUFFER_FLAGS, pt_flags(PAGE, a, 0, 1 << 5))  # reserved
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_DMABUF, bytearray(32))  # zero pages
        expect_errno(errno.ENOTTY, fcntl.ioctl, fd, ioc(0, G, 99, 0))
        expect_errno(errno.ENOTTY, fcntl.ioctl, fd, ioc(0, 0x42, 1, 0))
        assert int(sysfs("node_0_num_mapped_pages")) == 0
    finally:
        os.close(fd)


@test
def coherent_buffer_lifecycle():
    fd = rw()
    try:
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, CONFIG_COHERENT, coherent(1, 0))
        expect_errno(errno.ENOMEM, fcntl.ioctl, fd, CONFIG_COHERENT, coherent(1, 17 * PAGE))
        buf = coherent(1, 0x4000)
        fcntl.ioctl(fd, CONFIG_COHERENT, buf)
        assert struct.unpack_from("<Q", buf, 24)[0] == COHERENT_OFFSET
        expect_errno(errno.EBUSY, fcntl.ioctl, fd, CONFIG_COHERENT, coherent(1, 0x4000))
        m = mmap.mmap(fd, 0x4000, flags=mmap.MAP_SHARED, offset=COHERENT_OFFSET)
        m[:8] = b"apex-rs!"
        assert m[:8] == b"apex-rs!"
        # libedgetpu maps the host queue it placed in coherent memory.
        fcntl.ioctl(fd, MAP_BUFFER, pt(size=0x4000, host=addr_of(m), dev=0x200000))
        assert int(sysfs("node_0_num_mapped_pages")) == 4
        # Freeing while mapped is safe: the device mapping is removed and user
        # mappings are revoked before the memory goes (gasket freed it in place).
        expect_errno(errno.EADDRNOTAVAIL, fcntl.ioctl, fd, CONFIG_COHERENT, coherent(0, 0x4000, 0x1234))
        fcntl.ioctl(fd, CONFIG_COHERENT, coherent(0, 0x4000, COHERENT_OFFSET))
        assert int(sysfs("node_0_num_mapped_pages")) == 0
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, UNMAP_BUFFER, pt(size=0x4000, dev=0x200000))
        # The stale VMA cannot be mapped into the device again.
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_BUFFER, pt(size=0x4000, host=addr_of(m), dev=0x200000))
        m.close()
        # Mapping beyond an allocation, or with none, fails.
        expect_errno(errno.EINVAL, mmap.mmap, fd, PAGE, flags=mmap.MAP_SHARED, offset=COHERENT_OFFSET)
    finally:
        os.close(fd)


@test
def revoked_coherent_mapping_faults():
    pid = os.fork()
    if pid == 0:
        fd = rw()
        fcntl.ioctl(fd, CONFIG_COHERENT, coherent(1, 0x4000))
        m = mmap.mmap(fd, 0x4000, flags=mmap.MAP_SHARED, offset=COHERENT_OFFSET)
        m[0] = 7
        fcntl.ioctl(fd, CONFIG_COHERENT, coherent(0, 0x4000, COHERENT_OFFSET))
        ctypes.c_uint8.from_address(addr_of(m)).value  # must fault
        os._exit(0)
    _, st = os.waitpid(pid, 0)
    assert os.WIFSIGNALED(st) and os.WTERMSIG(st) == signal.SIGBUS, f"status {st:#x}"
    assert int(sysfs("is_device_owned")) == 0


@test
def csr_windows_map_and_misuse_is_refused():
    fd = rw()
    try:
        m = mmap.mmap(fd, PAGE, flags=mmap.MAP_SHARED, offset=0x48000)
        paused = ctypes.c_uint64.from_address(addr_of(m) + 0x6E0).value
        assert paused in (0, 1), hex(paused)
        m.close()
        for off in (0x44000, 0x40000):
            mmap.mmap(fd, PAGE, flags=mmap.MAP_SHARED, offset=off).close()
        expect_errno(errno.EINVAL, mmap.mmap, fd, 2 * PAGE, flags=mmap.MAP_SHARED, offset=0x40000)
        expect_errno(errno.EINVAL, mmap.mmap, fd, PAGE, flags=mmap.MAP_SHARED, offset=0x41000)
        expect_errno(errno.EINVAL, mmap.mmap, fd, PAGE, flags=mmap.MAP_SHARED, offset=0)
        expect_errno(errno.EINVAL, mmap.mmap, fd, PAGE, flags=mmap.MAP_PRIVATE, offset=0x48000)
        expect_errno(errno.EPERM, mmap.mmap, fd, PAGE, flags=mmap.MAP_SHARED,
                     prot=mmap.PROT_READ | mmap.PROT_EXEC, offset=0x48000)
    finally:
        os.close(fd)
    # A read-only opener may map registers read-only, never writable.
    ro = os.open(DEV, os.O_RDONLY)
    try:
        m = mmap.mmap(ro, PAGE, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ, offset=0x48000)
        m.close()
    finally:
        os.close(ro)


@test
def eventfds_and_clock_ioctls():
    fd = rw()
    try:
        efd = libc.eventfd(0, 0)
        assert efd >= 0
        fcntl.ioctl(fd, SET_EVENTFD, bytearray(struct.pack("<2Q", 12, efd)))
        fcntl.ioctl(fd, SET_EVENTFD, bytearray(struct.pack("<2Q", 0, efd)))  # replace
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, SET_EVENTFD, bytearray(struct.pack("<2Q", 13, efd)))
        expect_errno(errno.EBADF, fcntl.ioctl, fd, SET_EVENTFD, bytearray(struct.pack("<2Q", 1, 9999)))
        fcntl.ioctl(fd, CLEAR_EVENTFD, 12)
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, CLEAR_EVENTFD, 13)
        os.close(efd)
        fcntl.ioctl(fd, CLEAR_INTERRUPT_COUNTS)
        for perf in range(4):
            fcntl.ioctl(fd, PERFORMANCE, bytearray(struct.pack("<I", perf)))
        assert sysfs("throttle").startswith("level 0 divider 0"), sysfs("throttle")
        fcntl.ioctl(fd, PERFORMANCE, bytearray(struct.pack("<I", 0)))
        assert "divider 3 programmed 3" in sysfs("throttle"), sysfs("throttle")
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, PERFORMANCE, bytearray(struct.pack("<I", 4)))
        fcntl.ioctl(fd, GATE_CLOCK, bytearray(16))
        fcntl.ioctl(fd, RESET)
        assert sysfs("status") == "ALIVE"
    finally:
        os.close(fd)


@test
def killed_owner_leaves_no_state():
    pid = os.fork()
    if pid == 0:
        fd = rw()
        m = anon(64)
        fcntl.ioctl(fd, MAP_BUFFER, pt(size=64 * PAGE, host=addr_of(m), dev=0))
        fcntl.ioctl(fd, CONFIG_COHERENT, coherent(1, 0x4000))
        c = mmap.mmap(fd, 0x4000, flags=mmap.MAP_SHARED, offset=COHERENT_OFFSET)
        c[0] = 1
        os.kill(os.getpid(), signal.SIGKILL)
    os.waitpid(pid, 0)
    for _ in range(50):
        if int(sysfs("is_device_owned")) == 0:
            break
        time.sleep(0.05)
    assert int(sysfs("is_device_owned")) == 0
    assert int(sysfs("node_0_num_mapped_pages")) == 0
    fd = rw()
    fcntl.ioctl(fd, CONFIG_COHERENT, coherent(1, 0x4000))  # previous one was released
    os.close(fd)


@test
def map_unmap_stress():
    fd = rw()
    try:
        fcntl.ioctl(fd, PARTITION, pt(size=2048))
        bufs = [anon(n) for n in (1, 7, 64, 513)]
        for i in range(2000):
            b = bufs[i % len(bufs)]
            n = len(b) // PAGE
            dev = (EXTENDED | (i % 6000) << 21) if i % 2 else ((i * 3) % 1000) << 12
            fcntl.ioctl(fd, MAP_BUFFER, pt(size=n * PAGE, host=addr_of(b), dev=dev))
            fcntl.ioctl(fd, UNMAP_BUFFER, pt(size=n * PAGE, dev=dev))
        assert int(sysfs("node_0_num_mapped_pages")) == 0
    finally:
        os.close(fd)


@test
def thermal_throttle_follows_trip_points():
    def write(attr, v):
        with open(f"{SYS}/{attr}", "w") as f:
            f.write(str(v))

    def wait_level(level):
        for _ in range(40):
            if sysfs("throttle").startswith(f"level {level} "):
                return
            time.sleep(0.05)
        raise AssertionError(f"throttle stayed at {sysfs('throttle')!r}, wanted level {level}")

    fd = rw()  # keep the GCB out of reset so the divider is observable
    try:
        fcntl.ioctl(fd, PERFORMANCE, bytearray(struct.pack("<I", 3)))  # MAX
        write("temp_poll_interval", 20)
        write("trip_point0_temp", 20000)  # below the current temperature
        wait_level(1)
        assert "divider 1 programmed 1" in sysfs("throttle"), sysfs("throttle")
        # A user request slower than the thermal limit wins.
        fcntl.ioctl(fd, PERFORMANCE, bytearray(struct.pack("<I", 0)))  # LOW
        assert "divider 3 programmed 3" in sysfs("throttle"), sysfs("throttle")
        fcntl.ioctl(fd, PERFORMANCE, bytearray(struct.pack("<I", 3)))
        assert "divider 1 programmed 1" in sysfs("throttle"), sysfs("throttle")
        write("trip_point0_temp", 85000)
        wait_level(0)
        assert "divider 0 programmed 0" in sysfs("throttle"), sysfs("throttle")
        # Out-of-order and malformed trip points are refused.
        expect_errno(errno.EINVAL, write, "trip_point0_temp", 99000)
        expect_errno(errno.EINVAL, write, "trip_point1_temp", "abc")
        expect_errno(errno.EINVAL, write, "temp_poll_interval", -1)
    finally:
        write("trip_point0_temp", 85000)
        write("temp_poll_interval", 5000)
        os.close(fd)


UDMABUF_CREATE = ioc(W, ord("u"), 0x42, 24)


def udmabuf(pages):
    mfd = os.memfd_create("apex-test", os.MFD_ALLOW_SEALING)
    os.ftruncate(mfd, pages * PAGE)
    fcntl.fcntl(mfd, fcntl.F_ADD_SEALS, fcntl.F_SEAL_SHRINK)
    u = os.open("/dev/udmabuf", os.O_RDWR)
    try:
        arg = ctypes.create_string_buffer(struct.pack("<IIQQ", mfd, 0, 0, pages * PAGE), 24)
        fd = libc.ioctl(u, ctypes.c_ulong(UDMABUF_CREATE), arg)
        if fd < 0:
            raise OSError(ctypes.get_errno(), "UDMABUF_CREATE")
    finally:
        os.close(u)
        os.close(mfd)
    return fd


def dmabuf_req(fd, dev, pages, map_, flags=0):
    return bytearray(struct.pack("<QQiIII", 0, dev, fd, pages, map_, flags))


@test
def dmabuf_import():
    fd = rw()
    try:
        a = udmabuf(16)
        b = udmabuf(16)
        fcntl.ioctl(fd, MAP_DMABUF, dmabuf_req(a, 0x30000, 16, 1, 1 << 1))
        assert int(sysfs("node_0_num_mapped_pages")) == 16
        expect_errno(errno.EBUSY, fcntl.ioctl, fd, MAP_DMABUF, dmabuf_req(b, 0x38000, 16, 1))
        # Unmapping needs the same dma-buf, address and size.
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_DMABUF, dmabuf_req(b, 0x30000, 16, 0))
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_DMABUF, dmabuf_req(a, 0x30000, 8, 0))
        fcntl.ioctl(fd, MAP_DMABUF, dmabuf_req(a, 0x30000, 16, 0))
        assert int(sysfs("node_0_num_mapped_pages")) == 0
        # Too small, bad fd, not a dma-buf, DMA_NONE, zero pages.
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_DMABUF, dmabuf_req(a, 0, 17, 1))
        expect_errno(errno.EBADF, fcntl.ioctl, fd, MAP_DMABUF, dmabuf_req(9999, 0, 1, 1))
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_DMABUF, dmabuf_req(fd, 0, 1, 1))
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_DMABUF, dmabuf_req(a, 0, 1, 1, 3 << 1))
        expect_errno(errno.EINVAL, fcntl.ioctl, fd, MAP_DMABUF, dmabuf_req(a, 0, 0, 1))
        # Extended address, and a mapping left behind for close to clean up.
        fcntl.ioctl(fd, PARTITION, pt(size=4096))
        fcntl.ioctl(fd, MAP_DMABUF, dmabuf_req(b, EXTENDED | 3 << 21, 16, 1))
        fcntl.ioctl(fd, MAP_DMABUF, dmabuf_req(a, 0, 16, 1))
        # The dma-buf fds can go; the mapping keeps the buffers alive.
        os.close(a)
        os.close(b)
        assert int(sysfs("node_0_num_mapped_pages")) == 32
    finally:
        os.close(fd)
    assert int(sysfs("node_0_num_mapped_pages")) == 0


@test
def hwmon_reports_temperature():
    found = []
    for h in sorted(os.listdir("/sys/class/hwmon")):
        base = f"/sys/class/hwmon/{h}"
        if open(f"{base}/name").read().strip() != "apex":
            continue
        if os.path.basename(os.readlink(f"{base}/device")) == os.path.basename(os.readlink(f"{SYS}/device")):
            found.append(base)
    assert len(found) == 1, found
    t = int(open(f"{found[0]}/temp1_input").read())
    assert abs(t - int(sysfs("temp"))) < 3000, t
    assert int(open(f"{found[0]}/temp1_max").read()) == int(sysfs("trip_point0_temp"))
    assert int(open(f"{found[0]}/temp1_crit").read()) == int(sysfs("trip_point2_temp"))


for name, ok, why in results:
    print(f"{'PASS' if ok else 'FAIL'} {NAME} {name}{(': ' + why) if why else ''}")
failed = sum(not ok for _, ok, _ in results)
print(f"abi_test {NAME}: {len(results) - failed}/{len(results)} passed")
sys.exit(1 if failed else 0)
