#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# PID 1 of the test VM: mount the basics, run the tests, power off.
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
export PYTHONUNBUFFERED=1
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null
mount -t tmpfs tmp /tmp
mount -t tmpfs run /run
echo 1 > /proc/sys/kernel/sysrq
mode=$(sed -n 's/.*apex\.mode=\([a-z,]*\).*/\1/p' /proc/cmdline)
APEX_SOAK=$(sed -n 's/.*apex\.soak=\([0-9]*\).*/\1/p' /proc/cmdline)
export APEX_SOAK=${APEX_SOAK:-30}
mkdir -p /apex/out
/apex/run-tests.sh "${mode:-all}" 2>&1 | tee /apex/out/tests.log
sync
echo o > /proc/sysrq-trigger
sleep 10
