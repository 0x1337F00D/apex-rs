#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Stop the test VM started by run.sh (only that QEMU, identified by its pidfile).
pidfile=${APEX_VM_WORK:-/tmp/apex-rs-vm}/qemu.pid
[ -f "$pidfile" ] || { echo "no test VM running"; exit 0; }
pid=$(cat "$pidfile")
if grep -q apex.mode "/proc/$pid/cmdline" 2> /dev/null; then kill "$pid" && echo "stopped $pid"; else echo "pid $pid is not the test VM"; fi
