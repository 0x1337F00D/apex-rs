# apex-rs

A Rust-for-Linux kernel driver for the Google Coral Edge TPU on PCIe
(Apex, `1ac1:089a`), as found on the Coral M.2, Mini PCIe and Dual Edge TPU cards.

apex-rs replaces Google's out-of-tree `gasket` + `apex` C modules and keeps their
user-space ABI: the same `/dev/apex_N` ioctls and mmap layout and the same
`/sys/class/apex/apex_N` attributes. The unmodified libedgetpu runtime, pycoral,
the TensorFlow Lite delegate and applications such as Frigate run on it without
changes.

- Written against the upstream Rust-for-Linux abstractions (PCI, IRQ, DMA,
  misc device, devres, workqueue, pin-init); raw FFI is confined to one module.
- Fixes memory-safety, lifetime and robustness defects of the original driver
  (see [below](#differences-from-gasketapex)).
- Adds dma-buf import, system sleep support, hwmon, 32-bit user-space support
  and VFIO-passthrough robustness.
- Tested on real hardware, including on a KASAN/lockdep/DMA-API-debug kernel.

> **Status:** experimental. Developed and tested on Linux 7.3 (x86-64) with two
> Edge TPUs passed through to a KVM guest. Not yet tested on bare metal, on
> arm64 or on other kernel versions. Use at your own risk.

## Contents

- [Requirements](#requirements)
- [Building](#building)
- [Installing and using](#installing-and-using)
- [Architecture](#architecture)
- [Differences from gasket/apex](#differences-from-gasketapex)
- [Testing](#testing)
- [Limitations](#limitations)
- [License](#license)

## Requirements

- Linux 7.3 or newer with `CONFIG_RUST=y`. The Rust kernel abstractions change
  between releases; the driver is written against 7.3.
- Kernel headers that include the Rust metadata (`rust/*.rmeta`), as distribution
  kernels with Rust support ship them.
- **The exact `rustc` that built the kernel.** Rust kernel modules only load the
  kernel's crate metadata when compiled by the same compiler build. The kernel
  records it in `CONFIG_RUSTC_VERSION_TEXT` (see `/proc/config.gz`).
- `clang` and `ld.lld` (`LLVM=1` builds), as for the kernel itself.

For Arch-based kernels whose compiler came from Arch's `rust` package (for
example CachyOS), `toolchain.sh` fetches that exact compiler from the Arch Linux
Archive, verifies its signature with pacman's keyring and installs it under
`toolchain/`:

```bash
./toolchain.sh
```

On other distributions, point `RUSTC` at a matching compiler, for example one
installed with `rustup toolchain install <version>` if the kernel was built with
an upstream `rustc` release.

## Building

```bash
make modules
```

This builds `build/driver/apex.ko` against the running kernel. Variables:
`KDIR` (kernel build directory), `RUSTC`, `OUT`. Further targets:

| Target | Purpose |
| --- | --- |
| `make test` | host tests of the hardware-independent modules (`cargo test`) |
| `make clippy` | build with the kernel's Clippy configuration (`CLIPPY=1`) |
| `make fmt` / `make fmt-check` | format with the kernel's rustfmt configuration |
| `make check` | `fmt-check`, `test` and `clippy` |

## Installing and using

Make sure Google's `apex`/`gasket` modules are not loaded, then:

```bash
sudo insmod build/driver/apex.ko
```

Each TPU appears as `/dev/apex_N` and `/sys/class/apex/apex_N`. To use the
devices without root, install the udev rule and add users to the `apex` group:

```bash
sudo install -m 0644 udev/60-apex-rs.rules /etc/udev/rules.d/
```

### Module parameters

| Parameter | Default | Meaning |
| --- | --- | --- |
| `allow_power_save` | `true` | keep the TPU in low-power reset while the device is closed |
| `allow_hw_clock_gating` | `true` | let the hardware gate clocks when idle |
| `allow_sw_clock_gating` | `false` | honour `APEX_IOCTL_GATE_CLOCK` |
| `temp_poll_interval` | `5000` | thermal poll interval in ms (0 disables throttling) |
| `test_resume_state_loss` | `false` | testing only: treat every resume as a loss of chip state |

### sysfs and hwmon

`/sys/class/apex/apex_N` provides gasket's attributes: `temp`, `unique_id`,
`status`, `trip_point{0,1,2}_temp`, `hw_temp_warn{1,2}[_en]`,
`temp_poll_interval`, `interrupt_counts`, `node_0_*`, `is_device_owned`,
`device_owner`, `write_open_count`, `reset_count`, `driver_version` and
`framework_version`. `throttle` (new) shows the thermal level, the effective GCB
clock divider and the divider currently programmed.

Each TPU also registers a hwmon device named `apex` with `temp1_input`,
`temp1_max` (trip point 0) and `temp1_crit` (trip point 2), so `sensors` and other
monitoring tools pick it up.

### Thermal throttling

The driver polls the on-die sensor and slows the TPU clock at the trip points
(85, 90 and 95 °C by default). It steps back down only once the chip is 2 °C
below a trip point, and it never runs faster than user space requested through
`APEX_IOCTL_PERFORMANCE_EXPECTATION`. In sustained dual-TPU inference, Coral
cards without a heatsink can reach the first trip points.

### Suspend, resume and virtual machines

On resume the driver checks whether the chip kept the state it was programmed
with:

- **Closed device:** always re-initialised and put back into low-power reset.
- **Open device, state kept:** interrupts are re-armed and the owner continues.
- **Open device, state lost:** the owner's register and buffer mappings are
  revoked and its interrupt eventfds signalled, so libedgetpu fails immediately
  (SIGBUS) instead of waiting forever. The next open starts from a clean chip.

The chip re-initialises its MSI-X table after losing power. Under VFIO
passthrough (Proxmox, libvirt and other KVM setups), a guest's writes to that
table only reach the hypervisor's emulated copy, so after every reset and resume
the driver briefly clears and sets MSI-X Enable, which makes the hypervisor
rewrite the hardware table. Without this, a TPU in a VM receives no interrupts
after a suspend cycle.

## Architecture

| Module | Contents |
| --- | --- |
| `driver/regs.rs` | CSR layout, interrupt routing, mmap windows |
| `driver/abi.rs` | gasket/apex ioctl numbers and argument layouts (64- and 32-bit) |
| `driver/pagetable.rs` | the chip's two-level address-translation table, driving a hardware backend trait |
| `driver/thermal.rs` | sensor conversion, trip points, throttle policy with hysteresis |
| `driver/text.rs` | sysfs input parsing |
| `driver/hw.rs` | register sequences (reset, clocks, thermal, interrupt routing), page-table backend |
| `driver/device.rs` | per-device state, ownership, mappings, mmap, sleep handling |
| `driver/file.rs` | the misc device and ioctl dispatch |
| `driver/sysfs.rs` | the `apex` class and hwmon attributes |
| `driver/sys.rs` | safe wrappers for the kernel C APIs the Rust kernel crate does not cover yet |
| `driver/helpers.c` | one C helper for `pgprot_device()`, a macro Rust cannot call |
| `driver/apex_main.rs` | PCI probe and unbind, IRQ wiring, PM callbacks, module |

The first five modules use only `core`, so `hosttest/` compiles the same source
files for the host and tests them with `cargo test`.

**Safety.** `unsafe` code is concentrated in `sys.rs`, with a `SAFETY` comment on
every block. Every kernel resource (pinned pages, DMA mappings, dma-buf
attachments, eventfd references, IRQs, BAR mappings, class and hwmon devices) is
owned by a Rust value and released in `Drop`, so error paths unwind without
manual cleanup.

**Locking and lifetime.** All mutable device state is behind one mutex.
Holding it with the CSR BAR present proves the device is bound: unbind takes the
same lock and tears everything down before it completes. Open files outlive
unbind and then fail with `ENODEV`. The lock order is `mmap_lock` → device state
→ `i_mmap_rwsem`; page pinning and VMA lookups run before the state lock is
taken. Interrupt handlers take no locks: eventfds are swapped under RCU and
counters are atomic.

**Gaps in the Rust abstractions** (worked around in `sys.rs`, candidates for
upstream abstractions): long-term user page pinning, scatter-gather mapping of
user pages, eventfd, dma-buf import, device classes and class devices, hwmon,
`vm_operations_struct`, `unmap_mapping_range()`, PCI PM callbacks, and PCI device
IDs for vendors missing from `pci_ids.h`.

## Differences from gasket/apex

Defects of the original driver that apex-rs fixes:

- **Out-of-bounds write in extended mappings.** The bounds check counted
  `ceil(pages / 512)` subtables and ignored the offset into the first one, so a
  mapping that crossed one more subtable than counted walked past the
  translation table into kernel memory and unrelated registers.
- **Coherent buffer use-after-free and leak.** It could be freed while still
  mapped into user space or the device, and allocating it twice leaked the
  first buffer. Now a free first removes the device mappings and revokes the
  user mappings, the memory never outlives the device binding, and a second
  allocation returns `EBUSY`.
- **Page migration under DMA.** Pages were pinned one at a time with
  `get_user_pages_fast()`, which does not stop the kernel from migrating them.
  apex-rs pins with `FOLL_LONGTERM` and maps each buffer with one
  scatter-gather DMA mapping.
- **mprotect() bypass.** A non-owner's read-only register mapping could be made
  writable; `VM_MAYWRITE` is now cleared.
- **Unbind with open files.** Open files now get `ENODEV`, user mappings are
  revoked (SIGBUS on access) and all DMA is torn down while the device is still
  bound.
- **Imprecise unmap.** `UNMAP_BUFFER` accepted any range and could unmap parts
  of other buffers; it must now match one earlier mapping exactly.
- **Thermal policy.** The poll overwrote the clock divider every few seconds,
  undoing `PERFORMANCE_EXPECTATION`, and toggled the throttle level on every poll
  while the temperature sat on a trip point.
- **DMA segment limit.** The device translates every page itself, but the
  driver left the DMA layer's 64 KiB segment default in place, which larger
  scatter-gather runs violated.
- **32-bit user space.** `MAP_BUFFER_FLAGS` has a different ioctl number on i386,
  where its argument is 36 bytes; both encodings are accepted.
- **Smaller ones:** second-level tables in coherent memory instead of `GFP_DMA`
  pages with streaming syncs; eventfds of the last owner are released on close;
  atomic interrupt counters; reserved map flags are rejected; resets wait for
  the hardware tables and restore MSI-X every time, not only at probe.

New: dma-buf import (`GASKET_IOCTL_MAP_DMABUF`), system sleep support, hwmon,
the `throttle` attribute. Not carried over: gasket's DragonBoard MSI-X unmask
workaround and the `bypass_top_level` debug parameter.

## Testing

**Host tests** (`make test`) check the ioctl numbers and argument layouts
against values compiled from Google's `gasket.h`/`apex.h` for x86-64 and i386
(`hosttest/golden/`), the thermal policy, input parsing, and the page table
against a model of the chip's address translation, including randomized
comparison with a reference map. Regenerating the golden files needs Google's
gasket sources; see `hosttest/golden/abi_golden.c`.

**Hardware tests** (`vm/`) boot a kernel in QEMU/KVM with the TPUs passed through
by VFIO and run Google's own runtime inside a Debian guest built from Google's
release artifacts (libedgetpu from `edgetpu_runtime_20221024`, pycoral 2.0.0,
tflite_runtime 2.5.0.post1):

```bash
sudo vm/host-vfio.sh bind "$$"
```

```bash
vm/run.sh all
```

The first command binds the TPUs to `vfio-pci` and lifts the memory-lock limit
of the given shell (QEMU pins guest RAM); `sudo vm/host-vfio.sh unbind` gives
them back. The suites:

| Suite | What it checks |
| --- | --- |
| `abi` | every ioctl, malformed input, permissions and ownership, mappings across subtables, coherent buffer revocation, CSR mmap rules, eventfds, clocks, thermal throttling, dma-buf import, hwmon; plus a static i386 C program (`vm/compat/`) for the 32-bit ABI |
| `infer` | image classification on every TPU through libedgetpu and pycoral |
| `stress` | concurrent ioctls from many threads; parallel inference on all TPUs (soak length via `APEX_VM_CMDLINE=apex.soak=SECONDS`) |
| `fuzz` | 50,000 ioctls with random arguments and pointers, random mmaps |
| `pm` | suspend/resume with the device closed and with a live interpreter: the PM core's dry run and real S3; the forced state-loss path |
| `lifecycle` | unbind with live mappings, rebind, module reload |

`vm/debug-kernel/build.sh` builds a vanilla kernel with KASAN, lockdep,
PROVE_RCU, DMA-API debugging, debug objects and kmemleak for the same suites;
`run.sh` takes `APEX_VM_KERNEL`, `APEX_VM_KO` and `APEX_VM_MEM`.

Results on two Edge TPUs (Dual Edge TPU card), Linux 7.3.4:

- all suites pass on the distribution kernel and on the debug kernel, with no
  KASAN, lockdep, DMA-API or debug-object reports and no kmemleak findings;
- MobileNet v2 classification of the Coral test image: correct label, 3.8 ms
  median per inference (4.6 ms under KASAN);
- 10-minute parallel soak on both TPUs: about 117,000 inferences each, none
  misclassified.

`.github/workflows/ci.yml` runs the host tests, formatting and script checks on
every push; building the module needs a Rust-enabled kernel tree and the TPU
suites need hardware, so those run locally.

## Limitations

- Real suspend-to-idle has not been exercised: a KVM guest in s2idle has no wake
  source. The PM core's dry run and real S3 cover the same driver callbacks.
- Device DMA through an imported dma-buf has not been exercised; dma-buf mapping,
  validation and cleanup have.
- PM callbacks are installed by setting the driver's `dev_pm_ops` pointer at
  probe, until the Rust PCI abstraction offers them.
- CSR mmap requests must lie within one register window, which is how
  libedgetpu maps them; gasket also accepted ranges straddling windows.
- Up to 16 devices.

## License

GPL-2.0-only; see [LICENSE](LICENSE). The register sequences, constants and the
user-space ABI are derived from Google's gasket/apex driver (Copyright (C) 2018
Google, Inc., GPL-2.0).
