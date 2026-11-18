# SPDX-License-Identifier: GPL-2.0-only
# Out-of-tree build of the apex-rs kernel module.
#
# The kernel's Rust metadata only loads with the exact rustc that built the
# kernel, so RUSTC defaults to the pinned toolchain in toolchain/ (see README).

KDIR    ?= /usr/lib/modules/$(shell uname -r)/build
RUSTC   ?= $(CURDIR)/toolchain/bin/rustc
RUSTFMT ?= $(CURDIR)/toolchain/bin/rustfmt
CLIPPY_DRIVER ?= $(CURDIR)/toolchain/bin/clippy-driver
OUT     ?= $(CURDIR)/build/driver

KBUILD = $(MAKE) -C $(KDIR) M=$(CURDIR)/driver MO=$(OUT) LLVM=1 RUSTC=$(RUSTC)

.PHONY: modules clippy fmt fmt-check clean test check
modules:
	@mkdir -p $(OUT)
	$(KBUILD) modules

# Build with the kernel's Clippy configuration (kbuild's CLIPPY=1).
clippy:
	@mkdir -p $(OUT)
	touch driver/apex_main.rs
	$(KBUILD) CLIPPY=1 CLIPPY_DRIVER=$(CLIPPY_DRIVER) modules

# Format with the kernel's rustfmt configuration (.rustfmt.toml).
fmt:
	$(RUSTFMT) --config-path .rustfmt.toml driver/apex_main.rs hosttest/src/*.rs

fmt-check:
	$(RUSTFMT) --check --config-path .rustfmt.toml driver/apex_main.rs hosttest/src/*.rs

clean:
	$(KBUILD) clean
	rm -rf $(OUT)

# Host tests of the hardware-independent driver modules.
test:
	cd hosttest && cargo test

check: fmt-check test clippy
