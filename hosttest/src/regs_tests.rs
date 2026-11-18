// SPDX-License-Identifier: GPL-2.0-only

use crate::regs::*;

#[test]
fn mmap_accepts_exact_csr_windows() {
    for r in MAPPABLE_REGIONS {
        assert_eq!(
            classify_mmap(r.start as u64, r.len as u64, 0),
            Some(MmapTarget::Csr {
                bar_offset: r.start
            })
        );
    }
}

#[test]
fn mmap_rejects_straddling_and_foreign_ranges() {
    // Starts in a window, runs past it.
    assert_eq!(classify_mmap(0x40000, 0x2000, 0), None);
    // Covers the hole between windows.
    assert_eq!(classify_mmap(0x41000, 0x1000, 0), None);
    // BAR start, which gasket's mmap would partially map.
    assert_eq!(classify_mmap(0, 0x10_0000, 0), None);
    // Misaligned, empty and overflowing requests.
    assert_eq!(classify_mmap(0x40001, 0x1000, 0), None);
    assert_eq!(classify_mmap(0x40000, 0, 0), None);
    assert_eq!(classify_mmap(u64::MAX - 0xfff, 0x2000, 0), None);
    assert_eq!(classify_mmap(0x40000, 0x800, 0), None);
}

#[test]
fn mmap_coherent_needs_an_allocation_and_stays_inside_it() {
    assert_eq!(classify_mmap(COHERENT_MMAP_OFFSET, 0x1000, 0), None);
    assert_eq!(
        classify_mmap(COHERENT_MMAP_OFFSET, 0x4000, 0x4000),
        Some(MmapTarget::Coherent { offset: 0 })
    );
    assert_eq!(
        classify_mmap(COHERENT_MMAP_OFFSET + 0x1000, 0x1000, 0x4000),
        Some(MmapTarget::Coherent { offset: 0x1000 })
    );
    assert_eq!(classify_mmap(COHERENT_MMAP_OFFSET, 0x5000, 0x4000), None);
}

/// gasket_interrupt_setup() replayed literally: per interrupt, a read-modify-write
/// with mask `~(0xFFFF << shift)` for packed entries, a plain write otherwise.
fn gasket_routing(initial: impl Fn(usize) -> u64) -> Vec<(usize, u64)> {
    let mut regs: Vec<(usize, u64)> = Vec::new();
    let mut read = |regs: &Vec<(usize, u64)>, reg: usize| {
        regs.iter()
            .find(|(r, _)| *r == reg)
            .map(|(_, v)| *v)
            .unwrap_or(initial(reg))
    };
    for route in ROUTES {
        let value = match route.pack {
            None => route.vector as u64,
            Some(slot) => {
                let shift = slot as u32 * INTERRUPT_PACK_WIDTH;
                let mask = !(0xFFFFu64 << shift);
                (read(&regs, route.reg) & mask) | ((route.vector as u64) << shift)
            }
        };
        match regs.iter_mut().find(|(r, _)| *r == route.reg) {
            Some(e) => e.1 = value,
            None => regs.push((route.reg, value)),
        }
    }
    let _ = &mut read;
    regs
}

fn ours(initial: impl Fn(usize) -> u64) -> Vec<(usize, u64)> {
    route_register_values()
        .iter()
        .map(|&(reg, fields, packed)| {
            let v = if packed {
                (initial(reg) & PACKED_KEEP_MASK) | fields
            } else {
                fields
            };
            (reg, v)
        })
        .collect()
}

#[test]
fn interrupt_routing_equals_gasket_net_effect() {
    // From zero (post-reset) and from arbitrary garbage in the registers.
    for seed in [0u64, u64::MAX, 0xdead_beef_cafe_f00d, 0x5555_5555_5555_5555] {
        let init = |reg: usize| seed.rotate_left(reg as u32);
        assert_eq!(ours(init), gasket_routing(init), "seed {seed:#x}");
    }
}

#[test]
fn routing_covers_every_vector_once() {
    let mut seen = [false; NUM_INTERRUPTS];
    for (i, r) in ROUTES.iter().enumerate() {
        assert_eq!(r.vector as usize, i);
        assert!(!seen[i]);
        seen[i] = true;
    }
    let packed = route_register_values();
    assert_eq!(packed.len(), 7);
    // SC_HOST holds vectors 4..=7 packed.
    let sc = packed
        .iter()
        .find(|(r, _, _)| *r == KERNEL_HIB_SC_HOST_INTVECCTL)
        .unwrap();
    assert_eq!(sc.1, 4 | 5 << 7 | 6 << 14 | 7 << 21);
}

#[test]
fn rmw_matches_gasket_read_modify_write() {
    assert_eq!(rmw(0xffff_ffff, 0, 2, 14), 0xffff_3fff);
    assert_eq!(rmw(0, 0x3, 2, 14), 0xc000);
    // Value bits beyond the field width are dropped, not smeared.
    assert_eq!(rmw(0, 0xff, 2, 2), 0b1100);
    assert_eq!(rmw(0x1234, 7, 64, 0), 7);
}
