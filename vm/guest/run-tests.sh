#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Test sequence inside the VM. Modes (comma-separated): all, abi, infer, stress,
# fuzz, pm, lifecycle, smoke. "all" runs everything with a 30 s soak.
mode=${1:-all}
cd /apex
rc=0
step() { echo; echo "===== $*"; }

step "kernel $(uname -r), mode $mode"
lspci -nn -d 1ac1: || true
step "insmod apex.ko"
modinfo ./apex.ko | grep -E '^(name|version|vermagic|parm)' || true
insmod ./apex.ko || { echo "RESULT insmod failed"; dmesg | tail -40; exit 1; }
sleep 1
dmesg | grep -iE 'apex' | tail -20
ls -l /dev/apex_* 2>&1
ls /sys/class/apex/ 2>&1
for d in /sys/class/apex/*; do echo "$d: temp=$(cat $d/temp) id=$(cat $d/unique_id) status=$(cat $d/status)"; done

case $mode in smoke) ;; *)
    case ",$mode," in *,all,*|*,abi,*)
        for d in /dev/apex_*; do step "abi_test $d"; python3 abi_test.py "$d" || rc=1; done
        if [ -x ./compat_test ]; then step "32-bit compat ABI test"; ./compat_test || rc=1; fi ;;
    esac
    case ",$mode," in *,all,*|*,infer,*)
        step "inference (Google libedgetpu + pycoral)"; python3 infer.py 300 || rc=1
        for d in /sys/class/apex/*; do echo "$d interrupts:"; grep -v ': 0$' "$d/interrupt_counts"; done ;;
    esac
    case ",$mode," in *,all,*|*,stress,*)
        step "stress (threaded ioctls, parallel inference soak)"
        python3 stress_test.py "${APEX_SOAK:-30}" || rc=1 ;;
    esac
    case ",$mode," in *,all,*|*,fuzz,*)
        step "fuzz (random ioctls and mmaps)"; python3 fuzz_test.py 50000 || rc=1 ;;
    esac
    case ",$mode," in *,all,*|*,pm,*)
        step "system sleep (pm_test, s2idle, S3)"; python3 pm_test.py || rc=1 ;;
    esac
    case ",$mode," in *,all,*|*,lifecycle,*)
        step "lifecycle (unbind with live mappings, rebind, reload)"; python3 lifecycle_test.py /apex/apex.ko || rc=1
        step "inference after reload"; python3 infer.py 50 || rc=1 ;;
    esac
esac

if [ -w /sys/kernel/debug/kmemleak ] || mount -t debugfs none /sys/kernel/debug 2> /dev/null; then
    if [ -w /sys/kernel/debug/kmemleak ]; then
        step "kmemleak"
        echo scan > /sys/kernel/debug/kmemleak; sleep 6; echo scan > /sys/kernel/debug/kmemleak
        leaks=$(grep -c '^unreferenced object' /sys/kernel/debug/kmemleak || true)
        grep -B2 -A14 'apex' /sys/kernel/debug/kmemleak | head -60
        apex_leaks=$(grep -A14 '^unreferenced object' /sys/kernel/debug/kmemleak | grep -c 'apex' || true)
        echo "kmemleak: $leaks unreferenced objects, $apex_leaks with apex frames"
        [ "$apex_leaks" -eq 0 ] || rc=1
    fi
fi
step "rmmod"
rmmod apex || rc=1
step "dmesg"
dmesg | grep -iE 'apex|BUG|WARNING|Oops|rcu|vfio|DMAR|iommu' | tail -60
if dmesg | grep -qE 'BUG:|WARNING:|Oops|general protection|KASAN:|possible circular locking|DMA-API: .*apex'; then
    echo "kernel reported problems"; rc=1
fi
echo "RESULT rc=$rc"
