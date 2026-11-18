#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Hand the Edge TPUs to vfio-pci so the test VM can own them, or give them back.
#
#   sudo vm/host-vfio.sh bind   [PID]   # PID: process allowed to lock guest RAM
#   sudo vm/host-vfio.sh unbind
#
# bind:   loads vfio-pci, binds every 1ac1:089a device to it, makes their VFIO
#         group(s) accessible to the invoking user and, if PID is given, lifts
#         RLIMIT_MEMLOCK of that process (QEMU pins all guest RAM for DMA).
# unbind: releases the devices from vfio-pci and clears driver_override, so a
#         host driver can claim them again.
set -eu

[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }
user=${SUDO_USER:-root}
devices=$(for d in /sys/bus/pci/devices/*; do
    if [ "$(cat "$d/vendor")" = 0x1ac1 ] && [ "$(cat "$d/device")" = 0x089a ]; then basename "$d"; fi
done)
[ -n "$devices" ] || { echo "no Edge TPU (1ac1:089a) found" >&2; exit 1; }

case "${1:-}" in
bind)
    modprobe vfio-pci
    for d in $devices; do
        echo vfio-pci > "/sys/bus/pci/devices/$d/driver_override"
        if [ -e "/sys/bus/pci/devices/$d/driver" ]; then
            echo "$d" > "/sys/bus/pci/devices/$d/driver/unbind"
        fi
        echo "$d" > /sys/bus/pci/drivers_probe
        group=$(basename "$(readlink "/sys/bus/pci/devices/$d/iommu_group")")
        chown "$user" "/dev/vfio/$group"
        echo "$d -> $(basename "$(readlink "/sys/bus/pci/devices/$d/driver")"), /dev/vfio/$group owned by $user"
    done
    if [ -n "${2:-}" ]; then
        prlimit --pid "$2" --memlock=unlimited:unlimited
        echo "memlock of PID $2: $(prlimit --pid "$2" --memlock --noheadings --output SOFT)"
    fi
    ;;
unbind)
    for d in $devices; do
        if [ -e "/sys/bus/pci/devices/$d/driver" ]; then
            echo "$d" > "/sys/bus/pci/devices/$d/driver/unbind"
        fi
        echo > "/sys/bus/pci/devices/$d/driver_override"
        echo "$d released"
    done
    ;;
*)
    echo "usage: $0 bind [PID] | unbind" >&2
    exit 2
    ;;
esac
