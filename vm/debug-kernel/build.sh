#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Build the debug guest kernel in RAM: vanilla Linux 7.3.4 + debug.config, with
# the same rustc as the host kernel (toolchain/). Output: $KB/linux-7.3.4.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
KB=${APEX_KBUILD:-/tmp/apex-rs-kbuild}
tarball=$root/toolchain/dl/linux-7.3.4.tar.xz
mk="make -C $KB/linux-7.3.4 LLVM=1 RUSTC=$root/toolchain/bin/rustc BINDGEN=$HOME/.cargo/bin/bindgen -j$(nproc)"
mkdir -p "$KB/bin"
# The kernel build needs bc(1) for timeconst.h; use busybox's if bc is missing.
if ! command -v bc > /dev/null && busybox bc < /dev/null 2> /dev/null; then
    ln -sf "$(command -v busybox)" "$KB/bin/bc"
fi
export PATH="$KB/bin:$PATH"
[ -d "$KB/linux-7.3.4" ] || tar -C "$KB" -xf "$tarball"
$mk defconfig
$mk kvm_guest.config
cd "$KB/linux-7.3.4"
./scripts/kconfig/merge_config.sh -m -O . .config "$here/debug.config" > /dev/null
cd - > /dev/null
$mk olddefconfig
$mk rustavailable
for opt in RUST KASAN PROVE_LOCKING DMA_API_DEBUG DEBUG_KMEMLEAK UDMABUF PM_DEBUG HWMON; do
    grep -q "^CONFIG_$opt=y" "$KB/linux-7.3.4/.config" || { echo "CONFIG_$opt did not stick" >&2; exit 1; }
done
$mk bzImage modules
echo "built: $KB/linux-7.3.4/arch/x86/boot/bzImage ($(cat "$KB/linux-7.3.4/include/config/kernel.release"))"
