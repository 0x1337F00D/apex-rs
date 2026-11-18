// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2018 Google, Inc.: derived from the gasket/apex driver.

//! Apex (Coral Edge TPU, "Beagle") CSR layout.
//!
//! All offsets are into PCI BAR 2. Values come from Google's gasket/apex driver
//! (`apex_driver.c`, framework 1.1.4, driver 1.2); this file only gives them names.
//! It is hardware-independent so the host tests can use it.

/// PCI vendor ID (Global Unichip Corp.).
pub(crate) const PCI_VENDOR_ID: u32 = 0x1ac1;
/// PCI device ID of the Apex chip.
pub(crate) const PCI_DEVICE_ID: u32 = 0x089a;

/// The CSR BAR.
pub(crate) const CSR_BAR: u32 = 2;
/// Bytes of BAR 2 the driver touches (and requires to be present).
pub(crate) const CSR_BAR_BYTES: usize = 0x10_0000;

/// Number of MSI-X vectors the chip uses (see [`Interrupt`]).
pub(crate) const NUM_INTERRUPTS: usize = 13;

/// Translation table entries in the kernel HIB page table.
pub(crate) const PAGE_TABLE_ENTRIES: usize = 8192;

pub(crate) const SCU_BASE: usize = 0x1A300;
pub(crate) const SCU_2: usize = SCU_BASE + 0x14;
pub(crate) const SCU_3: usize = SCU_BASE + 0x18;
pub(crate) const AXI_QUIESCE: usize = SCU_BASE + 0x2C;
pub(crate) const GCB_CLOCK_GATE: usize = SCU_BASE + 0x14;

pub(crate) const OMC0_D0: usize = 0x01a0d0;
pub(crate) const OMC0_D4: usize = 0x01a0d4;
pub(crate) const OMC0_D8: usize = 0x01a0d8;
pub(crate) const OMC0_DC: usize = 0x01a0dc;
pub(crate) const EFUSE_DC: usize = 0x01a2dc;
pub(crate) const EFUSE_E0: usize = 0x01a2e0;
pub(crate) const EFUSE_E4: usize = 0x01a2e4;
pub(crate) const EFUSE_E8: usize = 0x01a2e8;

pub(crate) const SCALAR_CORE_ERROR_STATUS: usize = 0x41a0;
pub(crate) const USER_HIB_ERROR_STATUS: usize = 0x86f0;

pub(crate) const KERNEL_HIB_PAGE_TABLE_SIZE: usize = 0x46000;
pub(crate) const KERNEL_HIB_EXTENDED_TABLE: usize = 0x46008;
pub(crate) const KERNEL_HIB_INSTR_QUEUE_INTVECCTL: usize = 0x46018;
pub(crate) const KERNEL_HIB_INPUT_ACTV_QUEUE_INTVECCTL: usize = 0x46020;
pub(crate) const KERNEL_HIB_PARAM_QUEUE_INTVECCTL: usize = 0x46028;
pub(crate) const KERNEL_HIB_OUTPUT_ACTV_QUEUE_INTVECCTL: usize = 0x46030;
pub(crate) const KERNEL_HIB_SC_HOST_INTVECCTL: usize = 0x46038;
pub(crate) const KERNEL_HIB_TOP_LEVEL_INTVECCTL: usize = 0x46040;
pub(crate) const KERNEL_HIB_FATAL_ERR_INTVECCTL: usize = 0x46048;
pub(crate) const KERNEL_HIB_PAGE_TABLE_INIT: usize = 0x46078;
pub(crate) const KERNEL_HIB_MSIX_TABLE_INIT: usize = 0x46080;
pub(crate) const USER_HIB_DMA_PAUSE: usize = 0x486D8;
pub(crate) const USER_HIB_DMA_PAUSED: usize = 0x486E0;
pub(crate) const IDLEGEN_IDLEREGISTER: usize = 0x4A000;
pub(crate) const KERNEL_HIB_PAGE_TABLE: usize = 0x50000;

/// SCU_3 bit: GCB is currently held in reset.
pub(crate) const SCU3_CUR_RST_GCB: u32 = 1 << 4;
/// SCU_3 bit: GCB RAM is shut down.
pub(crate) const SCU3_RAM_SHUTDOWN: u32 = 1 << 6;
pub(crate) const SCU3_PWR_STATE_OVR_SHIFT: u32 = 26;
/// SCU_3 bits 28..31: GCB clock divider (2 bits), AXI-fixed and 8051-fixed flags.
pub(crate) const SCU3_CLOCK_SHIFT: u32 = 28;

/// Width of one entry in a packed interrupt vector-control register.
pub(crate) const INTERRUPT_PACK_WIDTH: u32 = 7;

/// Polling budget for reset handshakes, as in apex_driver.c (`APEX_RESET_RETRY`).
pub(crate) const RESET_RETRIES: u32 = 120;
/// Delay between reset polls in milliseconds (`APEX_RESET_DELAY`).
pub(crate) const RESET_DELAY_MS: u32 = 100;

/// A user-mappable window of BAR 2. libedgetpu maps exactly these three pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Region {
    pub(crate) start: usize,
    pub(crate) len: usize,
}

/// Tile config 0, scalar core and user HIB register pages.
pub(crate) const MAPPABLE_REGIONS: [Region; 3] = [
    Region {
        start: 0x40000,
        len: 0x1000,
    },
    Region {
        start: 0x44000,
        len: 0x1000,
    },
    Region {
        start: 0x48000,
        len: 0x1000,
    },
];

/// mmap() offset of the coherent host-queue buffer (`APEX_CM_OFFSET`).
pub(crate) const COHERENT_MMAP_OFFSET: u64 = 0x100_0000;
/// Largest coherent buffer the driver hands out (`MAX_NUM_COHERENT_PAGES` = 16).
pub(crate) const COHERENT_MAX_BYTES: u64 = 16 * 4096;

/// Where an mmap() request at `offset` with `len` bytes goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MmapTarget {
    /// A CSR window: page offset into BAR 2.
    Csr { bar_offset: usize },
    /// The coherent buffer; `offset` is relative to its start.
    Coherent { offset: u64 },
}

/// Classify an mmap() request. The range must lie entirely within one CSR window or
/// within the coherent buffer of `coherent_len` bytes (0 if none is allocated).
///
/// Google's driver accepted ranges that straddle windows and left holes unmapped; a
/// range that touches anything outside a single window is rejected here instead.
pub(crate) fn classify_mmap(offset: u64, len: u64, coherent_len: u64) -> Option<MmapTarget> {
    if len == 0 || !offset.is_multiple_of(4096) || !len.is_multiple_of(4096) {
        return None;
    }
    let end = offset.checked_add(len)?;
    for r in MAPPABLE_REGIONS {
        let (start, rend) = (r.start as u64, (r.start + r.len) as u64);
        if offset >= start && end <= rend {
            return Some(MmapTarget::Csr {
                bar_offset: offset as usize,
            });
        }
    }
    if offset >= COHERENT_MMAP_OFFSET && end <= COHERENT_MMAP_OFFSET + coherent_len {
        return Some(MmapTarget::Coherent {
            offset: offset - COHERENT_MMAP_OFFSET,
        });
    }
    None
}

/// Interrupt sources are numbered by MSI-X vector, which is also the number
/// libedgetpu passes to `GASKET_IOCTL_SET_EVENTFD`: 0 instruction queue,
/// 1 input activations, 2 parameters, 3 output activations, 4..=7 scalar core to
/// host, 8..=11 top level, 12 fatal error.
/// Routing of one interrupt source into its vector-control register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) vector: u8,
    pub(crate) reg: usize,
    /// `None` for an unpacked register, `Some(n)` for slot `n` of a packed one.
    pub(crate) pack: Option<u8>,
}

/// Interrupt routing table, in vector order.
pub(crate) const ROUTES: [Route; NUM_INTERRUPTS] = [
    Route {
        vector: 0,
        reg: KERNEL_HIB_INSTR_QUEUE_INTVECCTL,
        pack: None,
    },
    Route {
        vector: 1,
        reg: KERNEL_HIB_INPUT_ACTV_QUEUE_INTVECCTL,
        pack: None,
    },
    Route {
        vector: 2,
        reg: KERNEL_HIB_PARAM_QUEUE_INTVECCTL,
        pack: None,
    },
    Route {
        vector: 3,
        reg: KERNEL_HIB_OUTPUT_ACTV_QUEUE_INTVECCTL,
        pack: None,
    },
    Route {
        vector: 4,
        reg: KERNEL_HIB_SC_HOST_INTVECCTL,
        pack: Some(0),
    },
    Route {
        vector: 5,
        reg: KERNEL_HIB_SC_HOST_INTVECCTL,
        pack: Some(1),
    },
    Route {
        vector: 6,
        reg: KERNEL_HIB_SC_HOST_INTVECCTL,
        pack: Some(2),
    },
    Route {
        vector: 7,
        reg: KERNEL_HIB_SC_HOST_INTVECCTL,
        pack: Some(3),
    },
    Route {
        vector: 8,
        reg: KERNEL_HIB_TOP_LEVEL_INTVECCTL,
        pack: Some(0),
    },
    Route {
        vector: 9,
        reg: KERNEL_HIB_TOP_LEVEL_INTVECCTL,
        pack: Some(1),
    },
    Route {
        vector: 10,
        reg: KERNEL_HIB_TOP_LEVEL_INTVECCTL,
        pack: Some(2),
    },
    Route {
        vector: 11,
        reg: KERNEL_HIB_TOP_LEVEL_INTVECCTL,
        pack: Some(3),
    },
    Route {
        vector: 12,
        reg: KERNEL_HIB_FATAL_ERR_INTVECCTL,
        pack: None,
    },
];

/// Bits of a packed vector-control register that survive programming.
///
/// gasket did four read-modify-writes with a 16-bit mask over 7-bit fields at shifts
/// 0, 7, 14 and 21. Their net effect is: bits 0..=27 hold the fields, bits 28..=36 are
/// cleared and bits 37..=63 keep their old value. Unpacked registers are written whole.
pub(crate) const PACKED_KEEP_MASK: u64 = !((1u64 << 37) - 1);

/// Compute the routing fields of every vector-control register.
///
/// Returns `(register, fields, packed)` triples, one per distinct register, in table
/// order. For packed registers the caller writes `(old & PACKED_KEEP_MASK) | fields`.
pub(crate) fn route_register_values() -> [(usize, u64, bool); 7] {
    let mut out = [(0usize, 0u64, false); 7];
    let mut n = 0;
    for route in ROUTES {
        let field_mask = (1u64 << INTERRUPT_PACK_WIDTH) - 1;
        let value = match route.pack {
            None => u64::from(route.vector),
            Some(slot) => {
                (u64::from(route.vector) & field_mask) << (u32::from(slot) * INTERRUPT_PACK_WIDTH)
            }
        };
        match out[..n].iter_mut().find(|(reg, _, _)| *reg == route.reg) {
            Some(entry) => entry.1 |= value,
            None => {
                out[n] = (route.reg, value, route.pack.is_some());
                n += 1;
            }
        }
    }
    out
}

/// Read-modify-write helper: replace `width` bits at `shift` in `old` with `value`.
pub(crate) const fn rmw(old: u64, value: u64, width: u32, shift: u32) -> u64 {
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    (old & !(mask << shift)) | ((value & mask) << shift)
}
