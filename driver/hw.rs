// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2018 Google, Inc.: derived from the gasket/apex driver.

//! Register-level sequences for the Apex chip.
//!
//! Every function takes a [`Csr`] borrowed from a bound device, so hardware can
//! only be touched while the driver owns it. The sequences follow
//! `apex_driver.c`; deviations are noted where they are made.

use crate::{
    pagetable,
    regs::{self, rmw},
    sys, thermal,
};
use kernel::{
    device::{self, Bound},
    dma::Coherent,
    io::{poll::read_poll_timeout, Io},
    pci,
    prelude::*,
    time::{delay::fsleep, Delta},
};

/// The CSR BAR, borrowed for the duration of a locked operation.
#[derive(Clone, Copy)]
pub(crate) struct Csr<'a> {
    bar: &'a pci::Bar<'a, { regs::CSR_BAR_BYTES }>,
}

impl<'a> Csr<'a> {
    pub(crate) fn new(bar: &'a pci::Bar<'a, { regs::CSR_BAR_BYTES }>) -> Self {
        Self { bar }
    }

    #[inline(always)]
    pub(crate) fn r32<const OFF: usize>(self) -> u32 {
        self.bar.read32(OFF)
    }
    #[inline(always)]
    pub(crate) fn w32<const OFF: usize>(self, v: u32) {
        self.bar.write32(v, OFF)
    }
    /// 64-bit read. The kernel's `Region` only promises 4-byte alignment, so
    /// 64-bit accessors are checked at run time; the `const` block proves at
    /// compile time that the check cannot fail (BARs are page aligned).
    #[inline(always)]
    pub(crate) fn r64<const OFF: usize>(self) -> u64 {
        const { assert!(OFF.is_multiple_of(8) && OFF + 8 <= regs::CSR_BAR_BYTES) };
        self.bar.try_read64(OFF).unwrap_or(u64::MAX)
    }
    #[inline(always)]
    pub(crate) fn w64<const OFF: usize>(self, v: u64) {
        const { assert!(OFF.is_multiple_of(8) && OFF + 8 <= regs::CSR_BAR_BYTES) };
        let _ = self.bar.try_write64(v, OFF);
    }
    /// gasket_read_modify_write_32().
    #[inline(always)]
    pub(crate) fn rmw32<const OFF: usize>(self, value: u32, width: u32, shift: u32) {
        let old = u64::from(self.r32::<OFF>());
        self.w32::<OFF>(rmw(old, u64::from(value), width, shift) as u32);
    }
    /// gasket_read_modify_write_64().
    #[inline(always)]
    pub(crate) fn rmw64<const OFF: usize>(self, value: u64, width: u32, shift: u32) {
        let old = self.r64::<OFF>();
        self.w64::<OFF>(rmw(old, value, width, shift));
    }

    /// Poll a 32-bit register until `reg & mask == want`, as
    /// gasket_wait_with_reschedule() did (100 ms steps, 12 s budget).
    fn wait32<const OFF: usize>(self, mask: u32, want: u32) -> Result {
        read_poll_timeout(
            || Ok(self.r32::<OFF>()),
            |v| v & mask == want,
            Delta::from_millis(i64::from(regs::RESET_DELAY_MS)),
            Delta::from_millis(i64::from(regs::RESET_DELAY_MS * regs::RESET_RETRIES)),
        )
        .map(|_| ())
    }

    fn wait64<const OFF: usize>(self, mask: u64, want: u64) -> Result {
        read_poll_timeout(
            || Ok(self.r64::<OFF>()),
            |v| v & mask == want,
            Delta::from_millis(i64::from(regs::RESET_DELAY_MS)),
            Delta::from_millis(i64::from(regs::RESET_DELAY_MS * regs::RESET_RETRIES)),
        )
        .map(|_| ())
    }
}

/// Whether the GCB (the TPU core) is held in reset.
pub(crate) fn gcb_in_reset(csr: Csr<'_>) -> bool {
    csr.r32::<{ regs::SCU_3 }>() & regs::SCU3_CUR_RST_GCB != 0
}

/// Quiesce DMA and put the GCB into reset with its RAM shut down (low power).
pub(crate) fn enter_reset(dev: &device::Device, csr: Csr<'_>) -> Result {
    // Force the GCB idle.
    csr.rmw64::<{ regs::IDLEGEN_IDLEREGISTER }>(0, 1, 32);
    // Pause DMA and wait for it to drain.
    csr.w64::<{ regs::USER_HIB_DMA_PAUSE }>(1);
    csr.wait64::<{ regs::USER_HIB_DMA_PAUSED }>(1, 1)
        .inspect_err(|_| {
            dev_err!(dev, "DMA did not quiesce\n");
        })?;
    // rg_rst_gcb, rg_gated_gcb, rg_force_ram_sd.
    csr.rmw32::<{ regs::SCU_2 }>(0x1, 2, 2);
    csr.rmw32::<{ regs::SCU_2 }>(0x1, 2, 18);
    csr.rmw32::<{ regs::SCU_3 }>(0x3, 2, 14);
    csr.wait32::<{ regs::SCU_3 }>(regs::SCU3_RAM_SHUTDOWN, regs::SCU3_RAM_SHUTDOWN)
        .inspect_err(|_| dev_err!(dev, "GCB RAM did not shut down\n"))
}

/// Take the GCB out of reset.
pub(crate) fn quit_reset(dev: &device::Device, csr: Csr<'_>, hw_clock_gating: bool) -> Result {
    csr.rmw32::<{ regs::SCU_3 }>(0x0, 2, 14);
    csr.rmw32::<{ regs::SCU_2 }>(0x0, 2, 18);
    csr.rmw32::<{ regs::SCU_2 }>(0x2, 2, 2);
    csr.wait32::<{ regs::SCU_3 }>(regs::SCU3_RAM_SHUTDOWN, 0)
        .inspect_err(|_| dev_err!(dev, "GCB RAM did not power up\n"))?;
    csr.wait32::<{ regs::SCU_3 }>(regs::SCU3_CUR_RST_GCB, 0)
        .inspect_err(|_| dev_err!(dev, "GCB did not leave reset\n"))?;
    // Power-state override: 3 = inactive and sleep disabled, 2 = inactive only.
    let ovr = if hw_clock_gating { 2 } else { 3 };
    csr.rmw32::<{ regs::SCU_3 }>(ovr, 2, regs::SCU3_PWR_STATE_OVR_SHIFT);
    Ok(())
}

/// apex_reset(): toggle reset if needed, then leave it.
pub(crate) fn reset(dev: &device::Device, csr: Csr<'_>, hw_clock_gating: bool) -> Result {
    if !gcb_in_reset(csr) {
        enter_reset(dev, csr)?;
    }
    quit_reset(dev, csr, hw_clock_gating)
}

/// Wait until the host interface has initialised its page table and MSI-X
/// table after reset. gasket only waited at probe; this also runs after the
/// resets on open and on `GASKET_IOCTL_RESET`.
pub(crate) fn wait_tables_ready(dev: &device::Device, csr: Csr<'_>) -> Result {
    read_poll_timeout(
        || {
            Ok(csr.r64::<{ regs::KERNEL_HIB_PAGE_TABLE_INIT }>() != 0
                && csr.r64::<{ regs::KERNEL_HIB_MSIX_TABLE_INIT }>() != 0)
        },
        |ready| *ready,
        Delta::from_millis(i64::from(regs::RESET_DELAY_MS)),
        Delta::from_millis(i64::from(regs::RESET_DELAY_MS * regs::RESET_RETRIES)),
    )
    .map(|_| ())
    .inspect_err(|_| dev_err!(dev, "page table / MSI-X table init timed out\n"))
}

/// Route every interrupt source to its MSI-X vector.
pub(crate) fn program_interrupt_routing(csr: Csr<'_>) -> Result {
    for (reg, fields, packed) in regs::route_register_values() {
        let value = if packed {
            (csr.bar.try_read64(reg)? & regs::PACKED_KEEP_MASK) | fields
        } else {
            fields
        };
        csr.bar.try_write64(value, reg)?;
    }
    Ok(())
}

/// Whether the interrupt routing registers still hold what
/// [`program_interrupt_routing`] wrote, i.e. the chip kept its state.
pub(crate) fn interrupt_routing_intact(csr: Csr<'_>) -> bool {
    regs::route_register_values()
        .iter()
        .all(|&(reg, fields, packed)| match csr.bar.try_read64(reg) {
            Ok(v) if packed => v & !regs::PACKED_KEEP_MASK == fields,
            Ok(v) => v == fields,
            Err(_) => false,
        })
}

/// Number of translation slots the hardware reports.
pub(crate) fn page_table_size(csr: Csr<'_>) -> u64 {
    csr.r64::<{ regs::KERNEL_HIB_PAGE_TABLE_SIZE }>()
}

/// Software clock gating (APEX_IOCTL_GATE_CLOCK).
pub(crate) fn gate_clock(csr: Csr<'_>, enable: bool) {
    if enable {
        csr.rmw32::<{ regs::AXI_QUIESCE }>(1, 1, 16);
        csr.rmw32::<{ regs::GCB_CLOCK_GATE }>(1, 2, 18);
    } else {
        csr.rmw32::<{ regs::GCB_CLOCK_GATE }>(0, 2, 18);
        csr.rmw32::<{ regs::AXI_QUIESCE }>(0, 1, 16);
    }
}

/// Program GCB, AXI and 8051 clocks (APEX_IOCTL_PERFORMANCE_EXPECTATION).
pub(crate) fn set_clocks(csr: Csr<'_>, divider: u32) {
    const AXI_CLK_FIXED: u32 = 0;
    // The 8051 is unused on PCIe, so its clock stays fixed.
    const MCU_CLK_FIXED: u32 = 1;
    let field = (divider & 3) | AXI_CLK_FIXED << 2 | MCU_CLK_FIXED << 3;
    csr.rmw32::<{ regs::SCU_3 }>(field, 4, regs::SCU3_CLOCK_SHIFT);
}

/// Change only the GCB clock divider (thermal throttling).
pub(crate) fn set_divider(csr: Csr<'_>, divider: u32) {
    csr.rmw32::<{ regs::SCU_3 }>(divider & 3, 2, regs::SCU3_CLOCK_SHIFT);
}

/// The GCB clock divider currently programmed.
pub(crate) fn divider(csr: Csr<'_>) -> u32 {
    (csr.r32::<{ regs::SCU_3 }>() >> regs::SCU3_CLOCK_SHIFT) & 3
}

/// Hardware temperature alarm configuration.
#[derive(Clone, Copy)]
pub(crate) struct TempWarn {
    pub(crate) adc: u32,
    pub(crate) enabled: bool,
}

/// Start the on-die temperature sensor.
pub(crate) fn enable_thermal_sensing(csr: Csr<'_>) {
    // Sensor clocks, then ENAD/ENVR/ENBG, then (100 us later) the controller.
    csr.rmw32::<{ regs::OMC0_D0 }>(0x1, 1, 7);
    csr.rmw32::<{ regs::OMC0_D8 }>(0x7, 3, 0);
    fsleep(Delta::from_micros(100));
    csr.rmw32::<{ regs::OMC0_DC }>(0x1, 1, 0);
}

/// Program both hardware alarms. warn1 drives INTR, warn2 drives SD_ALARM.
pub(crate) fn program_temp_warnings(csr: Csr<'_>, warn1: TempWarn, warn2: TempWarn) {
    csr.rmw32::<{ regs::OMC0_D4 }>(warn1.adc, 10, thermal::ADC_SHIFT);
    csr.rmw32::<{ regs::OMC0_D8 }>(warn2.adc, 10, thermal::ADC_SHIFT);
    csr.rmw32::<{ regs::OMC0_D4 }>(u32::from(warn1.enabled), 1, 31);
    csr.rmw32::<{ regs::OMC0_D8 }>(u32::from(warn2.enabled), 1, 31);
}

/// Current temperature as a raw ADC value.
pub(crate) fn temperature_adc(csr: Csr<'_>) -> u32 {
    thermal::adc_from_reg(csr.r32::<{ regs::OMC0_DC }>())
}

/// The chip's 128-bit eFuse ID, most significant word first.
pub(crate) fn unique_id(csr: Csr<'_>) -> [u32; 4] {
    [
        csr.r32::<{ regs::EFUSE_E8 }>(),
        csr.r32::<{ regs::EFUSE_E4 }>(),
        csr.r32::<{ regs::EFUSE_E0 }>(),
        csr.r32::<{ regs::EFUSE_DC }>(),
    ]
}

/// Error status registers, logged when the device is released.
pub(crate) fn error_status(csr: Csr<'_>) -> (u64, u64) {
    (
        csr.r64::<{ regs::USER_HIB_ERROR_STATUS }>(),
        csr.r64::<{ regs::SCALAR_CORE_ERROR_STATUS }>(),
    )
}

/// Second-level translation tables, one DMA-coherent page each.
///
/// gasket allocated these with `GFP_DMA` (the 16 MiB ISA zone on x86) and synced
/// streaming mappings after every update; coherent pages need neither.
pub(crate) struct Subtables {
    tables: KVVec<Option<Coherent<[u64]>>>,
}

impl Subtables {
    pub(crate) fn new() -> Result<Self> {
        let mut tables = KVVec::with_capacity(pagetable::TOTAL, GFP_KERNEL)?;
        for _ in 0..pagetable::TOTAL {
            tables.push(None, GFP_KERNEL)?;
        }
        Ok(Self { tables })
    }

    /// Release every table. Only valid once no slot references them.
    pub(crate) fn clear(&mut self) {
        for t in self.tables.iter_mut() {
            *t = None;
        }
    }
}

/// [`pagetable::Backend`] on real hardware.
pub(crate) struct TableBackend<'a> {
    pub(crate) dev: &'a device::Device<Bound>,
    pub(crate) csr: Csr<'a>,
    pub(crate) subtables: &'a mut Subtables,
}

impl pagetable::Backend for TableBackend<'_> {
    fn write_slot(&mut self, slot: usize, value: u64) {
        let off = regs::KERNEL_HIB_PAGE_TABLE + slot * 8;
        // `slot < TOTAL`, so the offset is inside the BAR.
        if self.csr.bar.try_write64(value, off).is_err() {
            dev_err!(self.dev, "page table slot {} out of range\n", slot);
        }
    }

    fn write_extended_start(&mut self, start: u64) {
        self.csr.w64::<{ regs::KERNEL_HIB_EXTENDED_TABLE }>(start);
    }

    fn alloc_subtable(&mut self, slot: usize) -> Result<u64, pagetable::Error> {
        let table =
            Coherent::<u64>::zeroed_slice(self.dev, pagetable::SUBTABLE_ENTRIES, GFP_KERNEL)
                .map_err(|_| pagetable::Error::NoMemory)?;
        let dma = table.dma_address();
        self.tables_mut()[slot] = Some(table);
        Ok(dma)
    }

    fn free_subtable(&mut self, slot: usize) {
        self.tables_mut()[slot] = None;
    }

    fn write_subtable_entry(&mut self, slot: usize, index: usize, value: u64) {
        if let Some(table) = &self.subtables.tables[slot] {
            sys::coherent_write_u64(table, index, value);
        }
    }

    fn subtable_barrier(&mut self) {
        sys::dma_wmb();
    }
}

impl TableBackend<'_> {
    fn tables_mut(&mut self) -> &mut KVVec<Option<Coherent<[u64]>>> {
        &mut self.subtables.tables
    }
}
