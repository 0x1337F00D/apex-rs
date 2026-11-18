#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Fetch the exact rustc the running (or given) kernel was built with into
# toolchain/, because Rust kernel modules only load metadata produced by that
# exact compiler build. Supports Arch-based kernels whose rustc came from Arch's
# "rust" package (as CachyOS does); verifies the package with pacman's keyring.
#
#   ./toolchain.sh [KDIR]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
kdir=${1:-/usr/lib/modules/$(uname -r)/build}
text=$(strings -n 8 "$kdir/rust/libcore.rmeta" | grep -m1 -o 'rustc [0-9][^)]*)[^@]*' || true)
pkgver=$(printf '%s' "$text" | sed -n 's/.*(Arch Linux rust \([^)]*\)).*/\1/p')
[ -n "$pkgver" ] || { echo "kernel rustc is not an Arch 'rust' package build: $text" >&2; exit 1; }
echo "kernel was built with: $text"
dl=$here/toolchain/dl
mkdir -p "$dl"
file="rust-$pkgver-x86_64.pkg.tar.zst"
url="https://archive.archlinux.org/packages/r/rust/$(printf '%s' "$file" | sed 's/:/%3A/')"
[ -f "$dl/$file" ] || curl -fL --retry 3 -o "$dl/$file" "$url"
[ -f "$dl/$file.sig" ] || curl -fL --retry 3 -o "$dl/$file.sig" "$url.sig"
gpgv --keyring /etc/pacman.d/gnupg/pubring.kbx "$dl/$file.sig" "$dl/$file"
dest=$here/toolchain/rust-$(printf %s "$pkgver" | tr : _)  # no ":" (LD_LIBRARY_PATH separator)
rm -rf "$dest" && mkdir -p "$dest"
tar -C "$dest" -xf "$dl/$file" usr
mkdir -p "$here/toolchain/bin"
cat > "$here/toolchain/bin/rustc" <<WRAP
#!/bin/sh
# Arch rust $pkgver, the compiler of the kernel in $kdir.
LD_LIBRARY_PATH="$dest/usr/lib\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}" exec "$dest/usr/bin/rustc" "\$@"
WRAP
chmod +x "$here/toolchain/bin/rustc"
"$here/toolchain/bin/rustc" --version
