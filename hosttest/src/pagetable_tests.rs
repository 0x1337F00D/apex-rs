// SPDX-License-Identifier: GPL-2.0-only
//! Page-table tests against a model of the chip's address translation.
//!
//! `Hw` records what the driver would write into BAR 2 and into subtable memory
//! and translates device addresses the way the hardware does. The tests then
//! compare that translation with a plain map of what should be mapped.

use crate::pagetable::*;
use std::collections::{BTreeMap, HashMap};

const SUB_DMA_BASE: u64 = 0x7700_0000_0000;

struct Hw {
    slots: Vec<u64>,
    extended_start: u64,
    subtables: HashMap<u64, Vec<u64>>, // dma address -> entries
    owner: HashMap<usize, u64>,        // slot -> dma of its subtable
    next_sub: u64,
    /// Fail the n-th upcoming subtable allocation (0 = the next one).
    fail_alloc_at: Option<usize>,
    allocs: usize,
    barriers: usize,
}

impl Hw {
    fn new() -> Self {
        Hw {
            slots: vec![0; TOTAL],
            extended_start: TOTAL as u64,
            subtables: HashMap::new(),
            owner: HashMap::new(),
            next_sub: 0,
            fail_alloc_at: None,
            allocs: 0,
            barriers: 0,
        }
    }

    /// What the device would resolve `dev_addr` to, or None if it faults.
    fn translate(&self, dev_addr: u64) -> Option<u64> {
        let offset = dev_addr & (PAGE_SIZE - 1);
        let entry = if dev_addr & EXTENDED_FLAG == 0 {
            let slot = (dev_addr >> 12) as usize;
            if slot >= self.extended_start as usize {
                return None;
            }
            self.slots[slot]
        } else {
            let addr = dev_addr & !EXTENDED_FLAG;
            let lvl0 = ((addr >> 21) & 0x1fff) as usize;
            let lvl1 = ((addr >> 12) & 0x1ff) as usize;
            let slot = self.extended_start as usize + lvl0;
            if slot >= TOTAL {
                return None;
            }
            let ptr = self.slots[slot];
            if ptr & VALID == 0 {
                return None;
            }
            self.subtables.get(&(ptr & !(PAGE_SIZE - 1)))?[lvl1]
        };
        (entry & VALID != 0).then(|| (entry & !(PAGE_SIZE - 1)) + offset)
    }
}

impl Backend for Hw {
    fn write_slot(&mut self, slot: usize, value: u64) {
        self.slots[slot] = value;
    }
    fn write_extended_start(&mut self, start: u64) {
        self.extended_start = start;
    }
    fn alloc_subtable(&mut self, slot: usize) -> Result<u64, Error> {
        if let Some(n) = self.fail_alloc_at {
            if n == 0 {
                self.fail_alloc_at = None;
                return Err(Error::NoMemory);
            }
            self.fail_alloc_at = Some(n - 1);
        }
        assert!(
            !self.owner.contains_key(&slot),
            "subtable for slot {slot} allocated twice"
        );
        let dma = SUB_DMA_BASE + self.next_sub * PAGE_SIZE;
        self.next_sub += 1;
        self.allocs += 1;
        self.subtables.insert(dma, vec![0; SUBTABLE_ENTRIES]);
        self.owner.insert(slot, dma);
        Ok(dma)
    }
    fn free_subtable(&mut self, slot: usize) {
        let dma = self
            .owner
            .remove(&slot)
            .expect("freeing a subtable that does not exist");
        assert_eq!(
            self.slots[slot], 0,
            "subtable freed while its slot still points at it"
        );
        self.subtables.remove(&dma);
    }
    fn write_subtable_entry(&mut self, slot: usize, index: usize, value: u64) {
        let dma = self.owner[&slot];
        self.subtables.get_mut(&dma).unwrap()[index] = value;
    }
    fn subtable_barrier(&mut self) {
        self.barriers += 1;
    }
}

fn table() -> Box<PageTable> {
    // SAFETY: PageTable consists only of integers and arrays of integers.
    let mut t: Box<PageTable> = unsafe { Box::new_zeroed().assume_init() };
    t.init();
    t
}

fn pages(base: u64, n: usize) -> Vec<u64> {
    (0..n as u64).map(|i| base + i * PAGE_SIZE).collect()
}

fn ext(lvl0: u64, lvl1: u64) -> u64 {
    EXTENDED_FLAG | lvl0 << 21 | lvl1 << 12
}

#[test]
fn simple_map_translate_unmap() {
    let (mut t, mut hw) = (table(), Hw::new());
    let r = t.validate(0x3000, 4 * PAGE_SIZE).unwrap();
    assert_eq!(r, Range::Simple { first: 3, count: 4 });
    t.map(&mut hw, r, &mut pages(0xa000_0000, 4).into_iter())
        .unwrap();
    assert_eq!(t.active_pages(), 4);
    assert_eq!(hw.translate(0x3000), Some(0xa000_0000));
    assert_eq!(hw.translate(0x6abc), Some(0xa000_3abc));
    assert_eq!(hw.translate(0x7000), None);
    assert!(t.is_mapped(r));
    t.unmap(&mut hw, r);
    assert_eq!(t.active_pages(), 0);
    assert_eq!(hw.translate(0x3000), None);
    assert!(t.is_free(r));
}

#[test]
fn malformed_requests_are_rejected() {
    let t = table();
    assert_eq!(t.validate(0, 0), Err(Error::Invalid));
    assert_eq!(t.validate(0, 100), Err(Error::Invalid));
    assert_eq!(t.validate(0, PAGE_SIZE + 1), Err(Error::Invalid));
    // Past the simple region.
    assert_eq!(
        t.validate((TOTAL as u64) << 12, PAGE_SIZE),
        Err(Error::Invalid)
    );
    assert_eq!(
        t.validate(((TOTAL - 1) as u64) << 12, 2 * PAGE_SIZE),
        Err(Error::Invalid)
    );
    // Enormous sizes must not wrap around.
    assert_eq!(t.validate(0x1000, u64::MAX - 0xfff), Err(Error::Invalid));
    // Extended addresses with nothing partitioned as extended.
    assert_eq!(t.validate(ext(0, 0), PAGE_SIZE), Err(Error::Invalid));
    // Bits above the 34-bit extended address space.
    assert_eq!(
        t.validate(EXTENDED_FLAG | 1 << 40, PAGE_SIZE),
        Err(Error::Invalid)
    );
    // Page offset bits are ignored, as in gasket.
    assert_eq!(
        t.validate(0x1fff, PAGE_SIZE),
        Ok(Range::Simple { first: 1, count: 1 })
    );
    assert_eq!(
        t.validate(((TOTAL - 1) as u64) << 12, PAGE_SIZE)
            .unwrap()
            .pages(),
        1
    );
}

#[test]
fn overlapping_map_is_busy_and_leaves_original_intact() {
    let (mut t, mut hw) = (table(), Hw::new());
    let a = t.validate(0x0, 8 * PAGE_SIZE).unwrap();
    t.map(&mut hw, a, &mut pages(0x1000_0000, 8).into_iter())
        .unwrap();
    let b = t.validate(0x7000, 2 * PAGE_SIZE).unwrap();
    assert_eq!(
        t.map(&mut hw, b, &mut pages(0x2000_0000, 2).into_iter()),
        Err(Error::Busy)
    );
    assert_eq!(hw.translate(0x7000), Some(0x1000_7000));
    assert_eq!(hw.translate(0x8000), None);
    assert_eq!(t.active_pages(), 8);
}

#[test]
fn short_or_long_page_lists_roll_back() {
    let (mut t, mut hw) = (table(), Hw::new());
    let r = t.validate(0x10_000, 4 * PAGE_SIZE).unwrap();
    assert_eq!(
        t.map(&mut hw, r, &mut pages(0x1000_0000, 3).into_iter()),
        Err(Error::Invalid)
    );
    assert!(t.is_free(r));
    assert!(hw.slots.iter().all(|&s| s == 0));
    assert_eq!(
        t.map(&mut hw, r, &mut pages(0x1000_0000, 5).into_iter()),
        Err(Error::Invalid)
    );
    assert!(hw.slots.iter().all(|&s| s == 0));
    // Misaligned DMA addresses are refused.
    let mut bad = pages(0x1000_0000, 4);
    bad[2] += 8;
    assert_eq!(t.map(&mut hw, r, &mut bad.into_iter()), Err(Error::Invalid));
    assert_eq!(t.active_pages(), 0);
    assert!(hw.slots.iter().all(|&s| s == 0));
}

#[test]
fn partition_moves_boundary_and_refuses_busy_slots() {
    let (mut t, mut hw) = (table(), Hw::new());
    t.partition(&mut hw, 4096).unwrap();
    assert_eq!((t.num_simple(), t.num_extended()), (4096, 4096));
    assert_eq!(hw.extended_start, 4096);
    let r = t.validate(4095 << 12, PAGE_SIZE).unwrap();
    t.map(&mut hw, r, &mut pages(0x1000_0000, 1).into_iter())
        .unwrap();
    // Shrinking the simple region over a used slot fails and changes nothing.
    assert_eq!(t.partition(&mut hw, 1024), Err(Error::Busy));
    assert_eq!(hw.extended_start, 4096);
    // Growing past it is fine.
    t.partition(&mut hw, 8000).unwrap();
    assert_eq!(hw.translate(4095 << 12), Some(0x1000_0000));
    assert_eq!(t.partition(&mut hw, TOTAL as u64 + 1), Err(Error::Invalid));
    assert_eq!(t.partition(&mut hw, u64::MAX), Err(Error::Invalid));
}

#[test]
fn extended_mapping_across_subtables() {
    let (mut t, mut hw) = (table(), Hw::new());
    t.partition(&mut hw, 7000).unwrap();
    // Starts at entry 500 of subtable 3 and spans into subtables 4 and 5.
    let n = 12 + 512 + 7;
    let r = t.validate(ext(3, 500), n as u64 * PAGE_SIZE).unwrap();
    assert_eq!(
        r,
        Range::Extended {
            lvl0: 3,
            lvl1: 500,
            count: n
        }
    );
    t.map(&mut hw, r, &mut pages(0x4000_0000, n).into_iter())
        .unwrap();
    assert_eq!(hw.allocs, 3);
    for i in 0..n as u64 {
        let addr = ext(3, 500) + i * PAGE_SIZE;
        assert_eq!(
            hw.translate(addr + 0x10),
            Some(0x4000_0000 + i * PAGE_SIZE + 0x10)
        );
    }
    assert_eq!(hw.translate(ext(3, 499)), None);
    assert_eq!(t.active_pages(), n);
    t.unmap(&mut hw, r);
    assert_eq!(hw.translate(ext(4, 0)), None);
    assert_eq!(t.active_pages(), 0);
    // Subtables are kept until gc, then all three are released.
    assert_eq!(hw.subtables.len(), 3);
    t.gc(&mut hw);
    assert!(hw.subtables.is_empty());
}

/// gasket bounds-checked `ceil(pages / 512)` subtables and ignored the start
/// offset, so this request passed and walked one slot past the table.
#[test]
fn regression_extended_range_past_last_subtable() {
    let (mut t, mut hw) = (table(), Hw::new());
    t.partition(&mut hw, (TOTAL - 1) as u64).unwrap(); // exactly one subtable
    assert_eq!(t.validate(ext(0, 500), 20 * PAGE_SIZE), Err(Error::Invalid));
    // The same size fits when it starts at the beginning of the subtable.
    let ok = t.validate(ext(0, 0), 20 * PAGE_SIZE).unwrap();
    t.map(&mut hw, ok, &mut pages(0x5000_0000, 20).into_iter())
        .unwrap();
    assert_eq!(hw.allocs, 1);
}

#[test]
fn failed_subtable_allocation_leaves_no_trace() {
    let (mut t, mut hw) = (table(), Hw::new());
    t.partition(&mut hw, 0).unwrap();
    // Pre-existing subtable 0 with its first entry in use.
    let keep = t.validate(ext(0, 0), PAGE_SIZE).unwrap();
    t.map(&mut hw, keep, &mut pages(0x6000_0000, 1).into_iter())
        .unwrap();
    let slots_before = hw.slots.clone();
    // Spans subtables 0..=2; 1 and 2 are new and the allocation for 2 fails.
    let n = 511 + 512 + 10;
    let r = t.validate(ext(0, 1), n as u64 * PAGE_SIZE).unwrap();
    hw.fail_alloc_at = Some(1);
    assert_eq!(
        t.map(&mut hw, r, &mut pages(0x7000_0000, n).into_iter()),
        Err(Error::NoMemory)
    );
    assert_eq!(hw.slots, slots_before);
    assert_eq!(hw.subtables.len(), 1);
    assert_eq!(hw.translate(ext(0, 0)), Some(0x6000_0000));
    assert_eq!(hw.translate(ext(0, 1)), None);
    // The table is still usable afterwards.
    t.map(&mut hw, r, &mut pages(0x7000_0000, n).into_iter())
        .unwrap();
    assert_eq!(
        hw.translate(ext(2, 9)),
        Some(0x7000_0000 + (n as u64 - 1) * PAGE_SIZE)
    );
    t.unmap(&mut hw, r);
    assert_eq!(t.active_pages(), 1);
}

#[test]
fn reset_releases_everything() {
    let (mut t, mut hw) = (table(), Hw::new());
    t.partition(&mut hw, 100).unwrap();
    let s = t.validate(0x5000, 10 * PAGE_SIZE).unwrap();
    t.map(&mut hw, s, &mut pages(0x1000_0000, 10).into_iter())
        .unwrap();
    let e = t.validate(ext(40, 3), 900 * PAGE_SIZE).unwrap();
    t.map(&mut hw, e, &mut pages(0x2000_0000, 900).into_iter())
        .unwrap();
    t.reset(&mut hw);
    assert_eq!(t.active_pages(), 0);
    assert_eq!(t.num_simple(), TOTAL);
    assert_eq!(hw.extended_start, TOTAL as u64);
    assert!(hw.subtables.is_empty());
    assert!(hw.slots.iter().all(|&s| s == 0));
}

/// Tiny deterministic PRNG so the randomized test needs no dependencies.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn randomized_against_reference_model() {
    for seed in 1..=20u64 {
        let mut rng = Lcg(seed);
        let (mut t, mut hw) = (table(), Hw::new());
        // dev page address -> dma page address
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();
        let mut live: Vec<(u64, u64)> = Vec::new(); // (dev_addr, bytes)
        let mut next_dma = 0x1_0000_0000u64;
        for _ in 0..400 {
            match rng.below(10) {
                0..=4 => {
                    let simple = rng.below(2) == 0;
                    let count = 1 + rng.below(if simple { 64 } else { 1200 });
                    let dev = if simple {
                        rng.below(TOTAL as u64) << 12
                    } else {
                        ext(rng.below(TOTAL as u64), rng.below(512))
                    };
                    let bytes = count * PAGE_SIZE;
                    let Ok(r) = t.validate(dev, bytes) else {
                        continue;
                    };
                    let list = pages(next_dma, count as usize);
                    next_dma += bytes;
                    let free = (0..count).all(|i| !model.contains_key(&(dev + i * PAGE_SIZE)));
                    let res = t.map(&mut hw, r, &mut list.clone().into_iter());
                    assert_eq!(res.is_ok(), free, "seed {seed}: map {dev:#x}+{count}");
                    if res.is_ok() {
                        for (i, dma) in list.into_iter().enumerate() {
                            model.insert(dev + i as u64 * PAGE_SIZE, dma);
                        }
                        live.push((dev, bytes));
                    }
                }
                5..=7 if !live.is_empty() => {
                    let (dev, bytes) = live.swap_remove(rng.below(live.len() as u64) as usize);
                    let r = t.validate(dev, bytes).unwrap();
                    assert!(t.is_mapped(r));
                    t.unmap(&mut hw, r);
                    for i in 0..bytes / PAGE_SIZE {
                        model.remove(&(dev + i * PAGE_SIZE));
                    }
                }
                8 => {
                    let n = rng.below(TOTAL as u64 + 1);
                    let res = t.partition(&mut hw, n);
                    if res.is_err() {
                        assert_eq!(res, Err(Error::Busy));
                    }
                }
                _ => {
                    if rng.below(8) == 0 {
                        t.reset(&mut hw);
                        model.clear();
                        live.clear();
                    } else {
                        t.gc(&mut hw);
                    }
                }
            }
            assert_eq!(t.active_pages(), model.len(), "seed {seed}");
            for (&dev, &dma) in &model {
                assert_eq!(hw.translate(dev), Some(dma), "seed {seed}: {dev:#x}");
            }
            // Probe some addresses that should not translate.
            for _ in 0..16 {
                let dev = if rng.below(2) == 0 {
                    rng.below(TOTAL as u64) << 12
                } else {
                    ext(rng.below(TOTAL as u64), rng.below(512))
                };
                if !model.contains_key(&dev) {
                    assert_eq!(hw.translate(dev), None, "seed {seed}: stray {dev:#x}");
                }
            }
        }
        t.reset(&mut hw);
        assert!(
            hw.subtables.is_empty() && hw.owner.is_empty(),
            "seed {seed}: leaked subtables"
        );
    }
}
