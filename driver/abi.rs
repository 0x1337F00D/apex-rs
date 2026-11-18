// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2018 Google, Inc.: derived from the gasket/apex driver.

//! The gasket/apex user-space ABI, as used by libedgetpu.
//!
//! Layouts and numbers match `gasket.h` and `apex.h` exactly, so existing
//! libedgetpu, pycoral and Frigate binaries work without a rebuild. This file is
//! hardware-independent; the host tests check every layout and number.

use core::mem::size_of;

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

/// The asm-generic `_IOC()` encoding (x86, arm64, riscv).
pub(crate) const fn ioc(dir: u32, ty: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | (ty << 8) | nr
}
const fn io(ty: u32, nr: u32) -> u32 {
    ioc(IOC_NONE, ty, nr, 0)
}
const fn iow<T>(ty: u32, nr: u32) -> u32 {
    ioc(IOC_WRITE, ty, nr, size_of::<T>())
}
const fn ior<T>(ty: u32, nr: u32) -> u32 {
    ioc(IOC_READ, ty, nr, size_of::<T>())
}
const fn iowr<T>(ty: u32, nr: u32) -> u32 {
    ioc(IOC_READ | IOC_WRITE, ty, nr, size_of::<T>())
}

/// `struct gasket_interrupt_eventfd`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InterruptEventfd {
    pub(crate) interrupt: u64,
    pub(crate) event_fd: u64,
}

/// `struct gasket_page_table_ioctl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PageTableIoctl {
    pub(crate) page_table_index: u64,
    pub(crate) size: u64,
    pub(crate) host_address: u64,
    pub(crate) device_address: u64,
}

/// `struct gasket_page_table_ioctl_flags`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PageTableIoctlFlags {
    pub(crate) base: PageTableIoctl,
    pub(crate) flags: u32,
    // Implicit tail padding in the C struct; spelled out so the type has no
    // uninitialized bytes when it is copied to or from user space.
    pub(crate) _pad: u32,
}

/// `struct gasket_coherent_alloc_config_ioctl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CoherentAllocConfig {
    pub(crate) page_table_index: u64,
    pub(crate) enable: u64,
    pub(crate) size: u64,
    pub(crate) dma_address: u64,
}

/// `struct gasket_page_table_ioctl_dmabuf`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PageTableIoctlDmabuf {
    pub(crate) page_table_index: u64,
    pub(crate) device_address: u64,
    pub(crate) dmabuf_fd: i32,
    pub(crate) num_pages: u32,
    pub(crate) map: u32,
    pub(crate) flags: u32,
}

/// `struct apex_gate_clock_ioctl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GateClock {
    pub(crate) enable: u64,
    pub(crate) force_idle: u64,
}

/// `struct apex_performance_expectation_ioctl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PerformanceExpectation {
    pub(crate) performance: u32,
}

pub(crate) const GASKET_IOCTL_BASE: u32 = 0xDC;
pub(crate) const APEX_IOCTL_BASE: u32 = 0x7F;

pub(crate) const GASKET_IOCTL_RESET: u32 = io(GASKET_IOCTL_BASE, 0);
pub(crate) const GASKET_IOCTL_SET_EVENTFD: u32 = iow::<InterruptEventfd>(GASKET_IOCTL_BASE, 1);
/// The argument is the interrupt number itself, not a pointer.
pub(crate) const GASKET_IOCTL_CLEAR_EVENTFD: u32 = iow::<u64>(GASKET_IOCTL_BASE, 2);
/// `CLEAR_EVENTFD` as encoded by 32-bit user space (`unsigned long` is 4 bytes).
pub(crate) const GASKET_IOCTL_CLEAR_EVENTFD_32: u32 = iow::<u32>(GASKET_IOCTL_BASE, 2);
pub(crate) const GASKET_IOCTL_LOOPBACK_INTERRUPT: u32 = iow::<u64>(GASKET_IOCTL_BASE, 3);
pub(crate) const GASKET_IOCTL_NUMBER_PAGE_TABLES: u32 = ior::<u64>(GASKET_IOCTL_BASE, 4);
pub(crate) const GASKET_IOCTL_PAGE_TABLE_SIZE: u32 = iowr::<PageTableIoctl>(GASKET_IOCTL_BASE, 5);
pub(crate) const GASKET_IOCTL_SIMPLE_PAGE_TABLE_SIZE: u32 =
    iowr::<PageTableIoctl>(GASKET_IOCTL_BASE, 6);
pub(crate) const GASKET_IOCTL_PARTITION_PAGE_TABLE: u32 =
    iow::<PageTableIoctl>(GASKET_IOCTL_BASE, 7);
pub(crate) const GASKET_IOCTL_MAP_BUFFER: u32 = iow::<PageTableIoctl>(GASKET_IOCTL_BASE, 8);
pub(crate) const GASKET_IOCTL_UNMAP_BUFFER: u32 = iow::<PageTableIoctl>(GASKET_IOCTL_BASE, 9);
pub(crate) const GASKET_IOCTL_CLEAR_INTERRUPT_COUNTS: u32 = io(GASKET_IOCTL_BASE, 10);
pub(crate) const GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR: u32 =
    iowr::<CoherentAllocConfig>(GASKET_IOCTL_BASE, 11);
pub(crate) const GASKET_IOCTL_MAP_BUFFER_FLAGS: u32 =
    iow::<PageTableIoctlFlags>(GASKET_IOCTL_BASE, 12);
/// `MAP_BUFFER_FLAGS` as encoded by 32-bit x86 user space, where `u64` is only
/// 4-byte aligned and the struct has no tail padding (36 bytes).
pub(crate) const GASKET_IOCTL_MAP_BUFFER_FLAGS_32: u32 = ioc(
    IOC_WRITE,
    GASKET_IOCTL_BASE,
    12,
    size_of::<PageTableIoctl>() + size_of::<u32>(),
);
/// `LOOPBACK_INTERRUPT` as encoded by 32-bit user space.
pub(crate) const GASKET_IOCTL_LOOPBACK_INTERRUPT_32: u32 = iow::<u32>(GASKET_IOCTL_BASE, 3);
pub(crate) const GASKET_IOCTL_MAP_DMABUF: u32 = iow::<PageTableIoctlDmabuf>(GASKET_IOCTL_BASE, 13);

pub(crate) const APEX_IOCTL_GATE_CLOCK: u32 = iow::<GateClock>(APEX_IOCTL_BASE, 0);
pub(crate) const APEX_IOCTL_PERFORMANCE_EXPECTATION: u32 =
    iow::<PerformanceExpectation>(APEX_IOCTL_BASE, 1);

/// DMA direction requested in `PageTableIoctlFlags::flags` bits 1..=2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Bidirectional,
    ToDevice,
    FromDevice,
}

impl Direction {
    /// Decode map flags. Bit 0 (status) is ignored as in gasket; reserved bits
    /// 3..=31 must be zero. `DMA_NONE` is rejected.
    pub(crate) fn from_flags(flags: u32) -> Option<Self> {
        if flags >> 3 != 0 {
            return None;
        }
        match (flags >> 1) & 0b11 {
            0 => Some(Self::Bidirectional),
            1 => Some(Self::ToDevice),
            2 => Some(Self::FromDevice),
            _ => None,
        }
    }

    /// Whether the device may write the memory (so pages must be pinned writable
    /// and dirtied on release).
    pub(crate) fn device_writes(self) -> bool {
        !matches!(self, Self::ToDevice)
    }
}

/// `enum apex_performance_expectation`, mapped to the GCB clock divider.
pub(crate) fn performance_clock_divider(performance: u32) -> Option<u32> {
    match performance {
        0 => Some(3), // LOW
        1 => Some(2), // MED
        2 => Some(1), // HIGH
        3 => Some(0), // MAX
        _ => None,
    }
}
