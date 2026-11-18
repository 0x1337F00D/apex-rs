// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2018 Google, Inc.: derived from the gasket/apex driver.

//! Apex address-translation table bookkeeping.
//!
//! The chip translates device virtual addresses through an 8192-slot table in
//! BAR 2. The slots are split into a *simple* region (slot = page) and an
//! *extended* region (slot = pointer to a host-resident 512-entry subtable):
//!
//! ```text
//! simple:   [ 63..25: 0 | 24..12: slot | 11..0: offset ]
//! extended: [ 63: 1 | 62..34: 0 | 33..21: lvl0 | 20..12: lvl1 | 11..0: offset ]
//! ```
//!
//! This module decides which slots a request touches, validates it, keeps the
//! occupancy state and drives a [`Backend`] that performs the actual register and
//! subtable writes. It does no I/O, allocation or locking of its own, so the
//! host tests can run it against a model of the hardware.
//!
//! Compared with gasket's `gasket_page_table.c`:
//! - extended ranges that cross subtables are bounds-checked against the number of
//!   subtables they really touch (gasket counted `ceil(pages / 512)` and could walk
//!   one slot past the end of the table, into unrelated registers);
//! - a failed map leaves no slot written and frees subtables it allocated;
//! - occupancy is tracked with bitmaps and per-subtable counters instead of a scan.

/// Device page size.
pub(crate) const PAGE_SHIFT: u32 = 12;
pub(crate) const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;
/// Entries per second-level table (one 4 KiB page of u64).
pub(crate) const SUBTABLE_ENTRIES: usize = 512;
/// Total first-level slots.
pub(crate) const TOTAL: usize = crate::regs::PAGE_TABLE_ENTRIES;
/// Bit that marks an extended device address.
pub(crate) const EXTENDED_FLAG: u64 = 1 << 63;
/// Valid bit OR-ed into every slot value handed to the device.
pub(crate) const VALID: u64 = 1;

const LVL0_SHIFT: u32 = 21;
const LVL0_WIDTH: u32 = 13;

const WORDS: usize = TOTAL / 64;
const SUB_WORDS: usize = SUBTABLE_ENTRIES / 64;

/// Page-table errors, mapped to errnos by the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// Malformed or out-of-range request (`EINVAL`).
    Invalid,
    /// Some slot in the range is already in use (`EBUSY`).
    Busy,
    /// The backend could not allocate a subtable (`ENOMEM`).
    NoMemory,
}

/// Hardware operations the table needs. Slot numbers are absolute (0..TOTAL).
pub(crate) trait Backend {
    /// Write a first-level slot register.
    fn write_slot(&mut self, slot: usize, value: u64);
    /// Program the first extended slot (= the number of simple slots).
    fn write_extended_start(&mut self, start: u64);
    /// Allocate a zeroed subtable for extended slot `slot` and return its DMA
    /// address. The backend owns the memory until [`Backend::free_subtable`].
    fn alloc_subtable(&mut self, slot: usize) -> Result<u64, Error>;
    /// Release the subtable of `slot`. The first-level slot is already cleared.
    fn free_subtable(&mut self, slot: usize);
    /// Write entry `index` of the subtable belonging to `slot`.
    fn write_subtable_entry(&mut self, slot: usize, index: usize, value: u64);
    /// Make preceding subtable writes visible to the device before the slot
    /// registers that reference them are used.
    fn subtable_barrier(&mut self) {}
}

/// A validated device range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Range {
    /// `count` simple slots starting at `first`.
    Simple { first: usize, count: usize },
    /// `count` pages starting at entry `lvl1` of extended slot `lvl0` (relative to
    /// the start of the extended region).
    Extended {
        lvl0: usize,
        lvl1: usize,
        count: usize,
    },
}

impl Range {
    #[allow(dead_code, reason = "used by the host tests")]
    pub(crate) fn pages(&self) -> usize {
        match *self {
            Range::Simple { count, .. } | Range::Extended { count, .. } => count,
        }
    }
}

/// Occupancy state of the translation table.
///
/// The struct is about 530 KiB and consists only of integers, so the driver
/// allocates it zeroed with vmalloc. A zeroed value must be passed through
/// [`PageTable::init`] before use.
pub(crate) struct PageTable {
    num_simple: usize,
    active_pages: usize,
    /// Bit per simple slot: in use.
    simple: [u64; WORDS],
    /// Bit per extended slot (absolute index): subtable allocated.
    sub_present: [u64; WORDS],
    /// Used entries per subtable (absolute index).
    sub_used: [u16; TOTAL],
    /// Bit per subtable entry: in use.
    sub_bits: [[u64; SUB_WORDS]; TOTAL],
}

fn test_bit(map: &[u64], bit: usize) -> bool {
    map[bit / 64] & (1 << (bit % 64)) != 0
}
fn set_bit(map: &mut [u64], bit: usize, on: bool) {
    if on {
        map[bit / 64] |= 1 << (bit % 64);
    } else {
        map[bit / 64] &= !(1 << (bit % 64));
    }
}

impl PageTable {
    /// Put the bookkeeping into its reset state: every slot simple and free.
    ///
    /// Does not touch the hardware; follow with [`PageTable::reset`] on a live
    /// device.
    pub(crate) fn init(&mut self) {
        self.num_simple = TOTAL;
        self.active_pages = 0;
        self.simple = [0; WORDS];
        self.sub_present = [0; WORDS];
        self.sub_used = [0; TOTAL];
        for sub in self.sub_bits.iter_mut() {
            *sub = [0; SUB_WORDS];
        }
    }

    pub(crate) fn total_entries(&self) -> usize {
        TOTAL
    }

    pub(crate) fn num_simple(&self) -> usize {
        self.num_simple
    }

    pub(crate) fn num_extended(&self) -> usize {
        TOTAL - self.num_simple
    }

    /// Device pages currently mapped.
    pub(crate) fn active_pages(&self) -> usize {
        self.active_pages
    }

    /// Validate `bytes` at `dev_addr` against the current partition.
    ///
    /// The low 12 bits of `dev_addr` are ignored, as gasket did. `bytes` must be a
    /// non-zero multiple of the page size.
    pub(crate) fn validate(&self, dev_addr: u64, bytes: u64) -> Result<Range, Error> {
        if bytes == 0 || !bytes.is_multiple_of(PAGE_SIZE) {
            return Err(Error::Invalid);
        }
        let count = usize::try_from(bytes / PAGE_SIZE).map_err(|_| Error::Invalid)?;
        if dev_addr & EXTENDED_FLAG == 0 {
            let first = usize::try_from(dev_addr >> PAGE_SHIFT).map_err(|_| Error::Invalid)?;
            let end = first.checked_add(count).ok_or(Error::Invalid)?;
            if first >= self.num_simple || end > self.num_simple {
                return Err(Error::Invalid);
            }
            Ok(Range::Simple { first, count })
        } else {
            let addr = dev_addr & !EXTENDED_FLAG;
            if addr >> (LVL0_SHIFT + LVL0_WIDTH) != 0 {
                return Err(Error::Invalid);
            }
            let lvl0 = (addr >> LVL0_SHIFT) as usize;
            let lvl1 = ((addr >> PAGE_SHIFT) as usize) & (SUBTABLE_ENTRIES - 1);
            // Subtables touched, counting the offset into the first one.
            let span = lvl1
                .checked_add(count)
                .ok_or(Error::Invalid)?
                .div_ceil(SUBTABLE_ENTRIES);
            let end = lvl0.checked_add(span).ok_or(Error::Invalid)?;
            if lvl0 >= self.num_extended() || end > self.num_extended() {
                return Err(Error::Invalid);
            }
            Ok(Range::Extended { lvl0, lvl1, count })
        }
    }

    /// Visit `(absolute slot, first entry, entries)` for each subtable chunk of
    /// an extended range.
    fn chunks(
        &self,
        lvl0: usize,
        lvl1: usize,
        count: usize,
        mut f: impl FnMut(usize, usize, usize),
    ) {
        let mut slot = self.num_simple + lvl0;
        let mut first = lvl1;
        let mut remain = count;
        while remain > 0 {
            let len = remain.min(SUBTABLE_ENTRIES - first);
            f(slot, first, len);
            remain -= len;
            first = 0;
            slot += 1;
        }
    }

    /// Whether every page of `range` is free.
    pub(crate) fn is_free(&self, range: Range) -> bool {
        match range {
            Range::Simple { first, count } => {
                (first..first + count).all(|s| !test_bit(&self.simple, s))
            }
            Range::Extended { lvl0, lvl1, count } => {
                let mut free = true;
                self.chunks(lvl0, lvl1, count, |slot, first, len| {
                    if test_bit(&self.sub_present, slot) {
                        free &= (first..first + len).all(|e| !test_bit(&self.sub_bits[slot], e));
                    }
                });
                free
            }
        }
    }

    /// Whether every page of `range` is in use.
    #[allow(dead_code, reason = "used by the host tests")]
    pub(crate) fn is_mapped(&self, range: Range) -> bool {
        match range {
            Range::Simple { first, count } => {
                (first..first + count).all(|s| test_bit(&self.simple, s))
            }
            Range::Extended { lvl0, lvl1, count } => {
                let mut mapped = true;
                self.chunks(lvl0, lvl1, count, |slot, first, len| {
                    mapped &= test_bit(&self.sub_present, slot)
                        && (first..first + len).all(|e| test_bit(&self.sub_bits[slot], e));
                });
                mapped
            }
        }
    }

    /// Map `range` to the DMA page addresses yielded by `pages` (in order).
    ///
    /// `pages` must yield exactly `range.pages()` page-aligned addresses. On error
    /// nothing stays mapped and subtables allocated by this call are released.
    pub(crate) fn map<B: Backend>(
        &mut self,
        be: &mut B,
        range: Range,
        pages: &mut dyn Iterator<Item = u64>,
    ) -> Result<(), Error> {
        if !self.is_free(range) {
            return Err(Error::Busy);
        }
        let mut new_subs = [0u64; WORDS];
        let result = self.map_inner(be, range, pages, &mut new_subs);
        if result.is_err() {
            self.unmap(be, range);
            self.release_new_subtables(be, &new_subs);
        }
        result
    }

    fn map_inner<B: Backend>(
        &mut self,
        be: &mut B,
        range: Range,
        pages: &mut dyn Iterator<Item = u64>,
        new_subs: &mut [u64; WORDS],
    ) -> Result<(), Error> {
        let mut next_page = || match pages.next() {
            Some(dma) if dma % PAGE_SIZE == 0 => Ok(dma),
            _ => Err(Error::Invalid),
        };
        match range {
            Range::Simple { first, count } => {
                for slot in first..first + count {
                    let dma = next_page()?;
                    be.write_slot(slot, dma | VALID);
                    set_bit(&mut self.simple, slot, true);
                    self.active_pages += 1;
                }
            }
            Range::Extended { lvl0, lvl1, count } => {
                // Allocate every missing subtable before any entry is written. A
                // fresh subtable is zeroed, so pointing its slot at it right away
                // exposes no stale translations.
                let first_slot = self.num_simple + lvl0;
                let span = (lvl1 + count).div_ceil(SUBTABLE_ENTRIES);
                for slot in first_slot..first_slot + span {
                    if test_bit(&self.sub_present, slot) {
                        continue;
                    }
                    let dma = be.alloc_subtable(slot)?;
                    set_bit(new_subs, slot, true);
                    set_bit(&mut self.sub_present, slot, true);
                    self.sub_used[slot] = 0;
                    self.sub_bits[slot] = [0; SUB_WORDS];
                    if dma % PAGE_SIZE != 0 {
                        return Err(Error::NoMemory);
                    }
                    be.write_slot(slot, dma | VALID);
                }
                let mut slot = first_slot;
                let mut first = lvl1;
                let mut remain = count;
                while remain > 0 {
                    let len = remain.min(SUBTABLE_ENTRIES - first);
                    for e in first..first + len {
                        let dma = next_page()?;
                        be.write_subtable_entry(slot, e, dma | VALID);
                        set_bit(&mut self.sub_bits[slot], e, true);
                        self.sub_used[slot] += 1;
                        self.active_pages += 1;
                    }
                    remain -= len;
                    first = 0;
                    slot += 1;
                }
                be.subtable_barrier();
            }
        }
        if pages.next().is_some() {
            // More pages than the range covers: a caller bug.
            return Err(Error::Invalid);
        }
        Ok(())
    }

    fn release_new_subtables<B: Backend>(&mut self, be: &mut B, new_subs: &[u64; WORDS]) {
        for slot in 0..TOTAL {
            if test_bit(new_subs, slot)
                && test_bit(&self.sub_present, slot)
                && self.sub_used[slot] == 0
            {
                self.free_sub(be, slot);
            }
        }
    }

    fn free_sub<B: Backend>(&mut self, be: &mut B, slot: usize) {
        be.write_slot(slot, 0);
        be.free_subtable(slot);
        set_bit(&mut self.sub_present, slot, false);
        self.sub_used[slot] = 0;
        self.sub_bits[slot] = [0; SUB_WORDS];
    }

    fn unmap_simple<B: Backend>(&mut self, be: &mut B, first: usize, count: usize) {
        for slot in first..first + count {
            be.write_slot(slot, 0);
            if test_bit(&self.simple, slot) {
                set_bit(&mut self.simple, slot, false);
                self.active_pages -= 1;
            }
        }
    }

    fn unmap_extended<B: Backend>(&mut self, be: &mut B, lvl0: usize, lvl1: usize, count: usize) {
        let mut slot = self.num_simple + lvl0;
        let mut first = lvl1;
        let mut remain = count;
        while remain > 0 {
            let len = remain.min(SUBTABLE_ENTRIES - first);
            if test_bit(&self.sub_present, slot) {
                for e in first..first + len {
                    be.write_subtable_entry(slot, e, 0);
                    if test_bit(&self.sub_bits[slot], e) {
                        set_bit(&mut self.sub_bits[slot], e, false);
                        self.sub_used[slot] -= 1;
                        self.active_pages -= 1;
                    }
                }
            }
            remain -= len;
            first = 0;
            slot += 1;
        }
        be.subtable_barrier();
    }

    /// Clear every page of `range`. Subtables stay allocated until [`Self::gc`],
    /// [`Self::partition`] or [`Self::reset`], as in gasket, because the device may
    /// still hold the first-level pointer while it is running.
    pub(crate) fn unmap<B: Backend>(&mut self, be: &mut B, range: Range) {
        match range {
            Range::Simple { first, count } => self.unmap_simple(be, first, count),
            Range::Extended { lvl0, lvl1, count } => self.unmap_extended(be, lvl0, lvl1, count),
        }
    }

    /// Free every empty subtable. Only call while the device is quiescent.
    pub(crate) fn gc<B: Backend>(&mut self, be: &mut B) {
        for slot in self.num_simple..TOTAL {
            if test_bit(&self.sub_present, slot) && self.sub_used[slot] == 0 {
                self.free_sub(be, slot);
            }
        }
    }

    /// Move the simple/extended boundary to `num_simple`.
    ///
    /// Fails with [`Error::Busy`] if a slot that changes role is in use, matching
    /// `gasket_page_table_partition()`.
    pub(crate) fn partition<B: Backend>(
        &mut self,
        be: &mut B,
        num_simple: u64,
    ) -> Result<(), Error> {
        let num_simple = usize::try_from(num_simple).map_err(|_| Error::Invalid)?;
        if num_simple > TOTAL {
            return Err(Error::Invalid);
        }
        self.gc(be);
        let start = self.num_simple.min(num_simple);
        for slot in start..TOTAL {
            let busy = if slot < self.num_simple {
                test_bit(&self.simple, slot)
            } else {
                test_bit(&self.sub_present, slot)
            };
            if busy {
                return Err(Error::Busy);
            }
        }
        self.num_simple = num_simple;
        be.write_extended_start(num_simple as u64);
        Ok(())
    }

    /// Unmap everything, free all subtables and make every slot simple again.
    pub(crate) fn reset<B: Backend>(&mut self, be: &mut B) {
        let num_simple = self.num_simple;
        self.unmap_simple(be, 0, num_simple);
        for slot in num_simple..TOTAL {
            if test_bit(&self.sub_present, slot) {
                self.active_pages -= self.sub_used[slot] as usize;
                self.sub_used[slot] = 0;
                self.free_sub(be, slot);
            } else {
                be.write_slot(slot, 0);
            }
        }
        self.num_simple = TOTAL;
        be.write_extended_start(TOTAL as u64);
    }
}
