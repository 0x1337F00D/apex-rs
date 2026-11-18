#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
# Build the static i386 compat test in a Debian bullseye i386 container.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out=$root/build/compat_test
mkdir -p "$root/build"
docker run --rm -v "$here:/c:ro" -v "$root/upstream/gasket-driver/src:/s:ro" -v "$root/build:/o" i386/debian:bullseye sh -c '
  printf "deb http://archive.debian.org/debian bullseye main\ndeb http://archive.debian.org/debian bullseye-updates main\ndeb http://archive.debian.org/debian-security bullseye-security main\n" > /etc/apt/sources.list
  apt-get update -qq && apt-get install -y -qq gcc libc6-dev > /dev/null 2>&1
  gcc -O2 -Wall -static -I/s -DDMA_TO_DEVICE_FLAG="(1u << 1)" -o /o/compat_test /c/compat_test.c'
file "$out"
