#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Install apex-rs for the running kernel, or remove it again.
#
#   sudo scripts/install.sh install     # after `make modules`
#   sudo scripts/install.sh uninstall
#
# install: releases any Edge TPU held by vfio-pci, installs build/driver/apex.ko
#          into /usr/lib/modules/<release>/updates/ (so udev loads it at boot
#          via its PCI alias), installs the udev rule, creates the "apex" group,
#          adds the invoking user to it and loads the module.
# uninstall: unloads the module and removes the module and the udev rule.
#
# The module only fits the kernel it was built for: rebuild and reinstall after
# every kernel update.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
ko=${APEX_KO:-$root/build/driver/apex.ko}
kver=$(uname -r)
dest=/usr/lib/modules/$kver/updates/apex.ko
rule=/etc/udev/rules.d/60-apex-rs.rules

[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }

edge_tpus() {
    for d in /sys/bus/pci/devices/*; do
        if [ "$(cat "$d/vendor")" = 0x1ac1 ] && [ "$(cat "$d/device")" = 0x089a ]; then
            basename "$d"
        fi
    done
}

case "${1:-}" in
install)
    [ -f "$ko" ] || { echo "$ko not found: run make modules first" >&2; exit 1; }
    built_for=$(modinfo -F vermagic "$ko" | cut -d' ' -f1)
    [ "$built_for" = "$kver" ] || { echo "module is for $built_for, running $kver: rebuild it" >&2; exit 1; }
    if lsmod | grep -q '^gasket '; then
        echo "Google's gasket/apex modules are loaded; remove them first" >&2
        exit 1
    fi
    if lsmod | grep -q '^apex '; then
        modprobe -r apex
    fi
    for d in $(edge_tpus); do
        drv=$(basename "$(readlink "/sys/bus/pci/devices/$d/driver" 2> /dev/null || echo none)")
        if [ "$drv" = vfio-pci ]; then
            echo "$d" > "/sys/bus/pci/devices/$d/driver/unbind"
            echo "released $d from vfio-pci"
        fi
        echo > "/sys/bus/pci/devices/$d/driver_override"
    done
    install -D -m 0644 "$ko" "$dest"
    depmod -a "$kver"
    install -D -m 0644 "$root/udev/60-apex-rs.rules" "$rule"
    getent group apex > /dev/null || groupadd --system apex
    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
        usermod -aG apex "$SUDO_USER"
        echo "added $SUDO_USER to group apex (takes effect at the next login)"
    fi
    udevadm control --reload
    modprobe apex
    udevadm settle
    for d in $(edge_tpus); do
        echo "$d -> $(basename "$(readlink "/sys/bus/pci/devices/$d/driver" 2> /dev/null || echo none)")"
    done
    ls -l /dev/apex_* 2> /dev/null || { echo "no /dev/apex_* device appeared" >&2; exit 1; }
    for c in /sys/class/apex/*; do
        echo "$(basename "$c"): $(cat "$c/status"), $(cat "$c/temp") m°C"
    done
    ;;
uninstall)
    if lsmod | grep -q '^apex '; then
        modprobe -r apex
    fi
    rm -f "$dest" "$rule"
    depmod -a "$kver"
    udevadm control --reload
    echo "removed apex-rs (the apex group was kept)"
    ;;
*)
    echo "usage: $0 install | uninstall" >&2
    exit 2
    ;;
esac
