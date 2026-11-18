#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Boot the host's own kernel in QEMU with the Edge TPUs passed through and run
# the guest tests against build/driver/apex.ko.
#
#   vm/host-vfio.sh bind <pid>   (once, as root; see README)
#   vm/run.sh [mode]             mode: all (default), abi, infer, lifecycle, smoke
#
# Disk image and scratch live in $APEX_VM_WORK (default /tmp/apex-rs-vm, tmpfs).
# The serial console and the test log are copied to vm/logs/<timestamp>/.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
mode=${1:-all}
work=${APEX_VM_WORK:-/tmp/apex-rs-vm}
ko=${APEX_VM_KO:-$root/build/driver/apex.ko}
kernel=${APEX_VM_KERNEL:-/usr/lib/modules/$(uname -r)/vmlinuz}
mem=${APEX_VM_MEM:-2048}
image=apex-rs-guest
stamp=$(date +%Y%m%d-%H%M%S)
logs=$here/logs/$stamp

[ -f "$ko" ] || { echo "build the module first (make modules)" >&2; exit 1; }
devices=$(for d in /sys/bus/pci/devices/*; do
    if [ "$(cat "$d/vendor")" = 0x1ac1 ] && [ "$(cat "$d/device")" = 0x089a ]; then basename "$d"; fi
done)
qdev=""
n=0
# APEX_VM_NO_TPU=1 boots without passthrough, to check the VM plumbing.
[ "${APEX_VM_NO_TPU:-0}" = 1 ] && devices=""
for d in $devices; do
    drv=$(basename "$(readlink "/sys/bus/pci/devices/$d/driver" 2>/dev/null || echo none)")
    [ "$drv" = vfio-pci ] || { echo "$d is bound to '$drv', not vfio-pci: run vm/host-vfio.sh bind" >&2; exit 1; }
    n=$((n + 1))
    qdev="$qdev -device pcie-root-port,id=rp$n,chassis=$n,slot=$n -device vfio-pci,host=$d,bus=rp$n"
done
[ -n "$qdev" ] || [ "${APEX_VM_NO_TPU:-0}" = 1 ] || { echo "no Edge TPU found" >&2; exit 1; }
want=$(( (mem + 160) * 1024 ))   # KiB of guest RAM plus slack that VFIO pins
limit=$(ulimit -l)
if [ -n "$qdev" ] && [ "$limit" != unlimited ] && [ "$limit" -lt "$want" ]; then
    echo "RLIMIT_MEMLOCK is ${limit} KiB; VFIO needs about ${want} KiB (vm/host-vfio.sh bind <pid>)" >&2
    exit 1
fi

mkdir -p "$work" "$logs"
id=$(docker image inspect -f '{{.Id}}' "$image" | cut -d: -f2 | cut -c1-16)
rootfs=$work/rootfs-$id.tar
if [ ! -f "$rootfs" ]; then
    cid=$(docker create "$image")
    docker export "$cid" > "$rootfs.tmp"
    docker rm -f "$cid" > /dev/null
    mv "$rootfs.tmp" "$rootfs"
fi
rm -rf "$work/stage" && mkdir -p "$work/stage/apex/out"
cp "$ko" "$here"/guest/*.py "$here"/guest/*.sh "$work/stage/apex/"
# Optional static i386 ABI test (vm/compat/build.sh).
[ -f "$root/build/compat_test" ] && cp "$root/build/compat_test" "$work/stage/apex/"
cp "$rootfs" "$work/disk.tar"
tar --append -f "$work/disk.tar" -C "$work/stage" --owner=0 --group=0 apex
rm -f "$work/disk.img"
truncate -s 3G "$work/disk.img"
mkfs.ext4 -q -F -L apexvm -d "$work/disk.tar" "$work/disk.img"
rm -f "$work/disk.tar"

{
    echo "date: $stamp"
    echo "host kernel: $(uname -r)"
    echo "guest kernel: $kernel"
    echo "module sha256: $(sha256sum "$ko" | cut -d' ' -f1)"
    echo "guest image: $image $id"
    echo "devices: $devices"
    echo "mode: $mode"
} > "$logs/manifest.txt"

echo "booting VM (mode $mode), serial log: $logs/serial.log"
# Wake the guest from S3: QEMU reports "suspended" and needs system_wakeup
# (the guest's RTC alarm does not reliably wake it).
(
    sleep 5
    while [ -f "$work/qemu.pid" ] && kill -0 "$(cat "$work/qemu.pid")" 2> /dev/null; do
        st=$(printf 'info status\n' | timeout 3 socat - "UNIX-CONNECT:$work/monitor.sock" 2> /dev/null | tr -d '\r')
        case $st in *"VM status: paused (suspended)"*)
            sleep 3
            printf 'system_wakeup\n' | timeout 3 socat - "UNIX-CONNECT:$work/monitor.sock" > /dev/null 2>&1
            echo "$(date +%T) woke guest from S3" >> "$logs/manifest.txt" ;;
        esac
        # After an S3 cycle the guest's ACPI power-off may not reach QEMU.
        if grep -aq 'reboot: Power down' "$logs/serial.log" 2> /dev/null; then
            sleep 2
            printf 'quit\n' | timeout 3 socat - "UNIX-CONNECT:$work/monitor.sock" > /dev/null 2>&1
        fi
        sleep 1
    done
) &
waker=$!
set +e
# shellcheck disable=SC2086
timeout "${APEX_VM_TIMEOUT:-1800}" qemu-system-x86_64 \
    -machine q35,accel=kvm -cpu host -smp 4 -m "$mem" \
    -global ICH9-LPC.disable_s3=0 \
    -display none -no-reboot -pidfile "$work/qemu.pid" \
    -monitor "unix:$work/monitor.sock,server,nowait" \
    -serial "file:$logs/serial.log" \
    -kernel "$kernel" \
    -append "console=ttyS0 root=/dev/vda rw rootfstype=ext4 init=/apex/init.sh panic=-1 no_console_suspend apex.mode=$mode ${APEX_VM_CMDLINE:-}" \
    -drive "file=$work/disk.img,format=raw,if=virtio" \
    $qdev
qrc=$?
kill "$waker" 2> /dev/null
set -e
debugfs -R "dump /apex/out/tests.log $logs/tests.log" "$work/disk.img" > /dev/null 2>&1 || true
echo "qemu exit $qrc" >> "$logs/manifest.txt"
grep -E '^(PASS|FAIL)|RESULT|passed$' "$logs/tests.log" 2>/dev/null || tail -40 "$logs/serial.log"
grep -q 'RESULT rc=0' "$logs/tests.log" 2>/dev/null
