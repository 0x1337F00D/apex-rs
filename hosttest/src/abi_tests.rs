// SPDX-License-Identifier: GPL-2.0-only
//! ABI layout and ioctl numbers, checked against values compiled from Google's
//! `gasket.h` / `apex.h` (see `golden/abi_golden.c`).

use crate::abi::*;
use core::mem::{align_of, size_of};

fn golden() -> Vec<(String, u64)> {
    parse(include_str!("../golden/abi_golden.txt"))
}

/// The same program built for i386 (`-m32` semantics: 4-byte aligned u64).
fn golden_i386() -> Vec<(String, u64)> {
    parse(include_str!("../golden/abi_golden_i386.txt"))
}

fn parse(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .map(|l| {
            let (name, value) = l.rsplit_once(' ').unwrap();
            let value = match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
                None => value.parse().unwrap(),
            };
            (name.to_string(), value)
        })
        .collect()
}

fn lookup(name: &str) -> u64 {
    golden()
        .into_iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("{name} missing from golden file"))
        .1
}

#[test]
fn ioctl_numbers_match_gasket_headers() {
    let ours = [
        ("GASKET_IOCTL_RESET", GASKET_IOCTL_RESET),
        ("GASKET_IOCTL_SET_EVENTFD", GASKET_IOCTL_SET_EVENTFD),
        ("GASKET_IOCTL_CLEAR_EVENTFD", GASKET_IOCTL_CLEAR_EVENTFD),
        (
            "GASKET_IOCTL_LOOPBACK_INTERRUPT",
            GASKET_IOCTL_LOOPBACK_INTERRUPT,
        ),
        (
            "GASKET_IOCTL_NUMBER_PAGE_TABLES",
            GASKET_IOCTL_NUMBER_PAGE_TABLES,
        ),
        ("GASKET_IOCTL_PAGE_TABLE_SIZE", GASKET_IOCTL_PAGE_TABLE_SIZE),
        (
            "GASKET_IOCTL_SIMPLE_PAGE_TABLE_SIZE",
            GASKET_IOCTL_SIMPLE_PAGE_TABLE_SIZE,
        ),
        (
            "GASKET_IOCTL_PARTITION_PAGE_TABLE",
            GASKET_IOCTL_PARTITION_PAGE_TABLE,
        ),
        ("GASKET_IOCTL_MAP_BUFFER", GASKET_IOCTL_MAP_BUFFER),
        ("GASKET_IOCTL_UNMAP_BUFFER", GASKET_IOCTL_UNMAP_BUFFER),
        (
            "GASKET_IOCTL_CLEAR_INTERRUPT_COUNTS",
            GASKET_IOCTL_CLEAR_INTERRUPT_COUNTS,
        ),
        (
            "GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR",
            GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR,
        ),
        (
            "GASKET_IOCTL_MAP_BUFFER_FLAGS",
            GASKET_IOCTL_MAP_BUFFER_FLAGS,
        ),
        ("GASKET_IOCTL_MAP_DMABUF", GASKET_IOCTL_MAP_DMABUF),
        ("APEX_IOCTL_GATE_CLOCK", APEX_IOCTL_GATE_CLOCK),
        (
            "APEX_IOCTL_PERFORMANCE_EXPECTATION",
            APEX_IOCTL_PERFORMANCE_EXPECTATION,
        ),
    ];
    for (name, value) in ours {
        assert_eq!(value as u64, lookup(name), "{name}");
    }
}

#[test]
fn struct_layouts_match_gasket_headers() {
    let ours = [
        (
            "sizeof gasket_interrupt_eventfd",
            size_of::<InterruptEventfd>(),
        ),
        (
            "sizeof gasket_page_table_ioctl",
            size_of::<PageTableIoctl>(),
        ),
        (
            "sizeof gasket_page_table_ioctl_flags",
            size_of::<PageTableIoctlFlags>(),
        ),
        (
            "sizeof gasket_coherent_alloc_config_ioctl",
            size_of::<CoherentAllocConfig>(),
        ),
        (
            "sizeof gasket_page_table_ioctl_dmabuf",
            size_of::<PageTableIoctlDmabuf>(),
        ),
        ("sizeof apex_gate_clock_ioctl", size_of::<GateClock>()),
        (
            "sizeof apex_performance_expectation_ioctl",
            size_of::<PerformanceExpectation>(),
        ),
    ];
    for (name, size) in ours {
        assert_eq!(size as u64, lookup(name), "{name}");
    }
    // 64-bit members on 64-bit boundaries, as gasket.h promises for 32/64 compat.
    assert_eq!(align_of::<PageTableIoctlFlags>(), 8);
    assert_eq!(core::mem::offset_of!(PageTableIoctlFlags, flags), 32);
    assert_eq!(core::mem::offset_of!(PageTableIoctlDmabuf, dmabuf_fd), 16);
    assert_eq!(core::mem::offset_of!(PageTableIoctlDmabuf, flags), 28);
}

#[test]
fn clear_eventfd_has_distinct_32bit_encoding() {
    assert_ne!(GASKET_IOCTL_CLEAR_EVENTFD, GASKET_IOCTL_CLEAR_EVENTFD_32);
    assert_eq!(GASKET_IOCTL_CLEAR_EVENTFD_32, 0x4004dc02);
}

#[test]
fn direction_flags_decode_like_gasket() {
    // libedgetpu: flags = direction << 1.
    assert_eq!(Direction::from_flags(0), Some(Direction::Bidirectional));
    assert_eq!(Direction::from_flags(1 << 1), Some(Direction::ToDevice));
    assert_eq!(Direction::from_flags(2 << 1), Some(Direction::FromDevice));
    // DMA_NONE is refused (gasket also refused it).
    assert_eq!(Direction::from_flags(3 << 1), None);
    // The status bit is ignored.
    assert_eq!(Direction::from_flags(1), Some(Direction::Bidirectional));
    // Reserved bits are refused so they can gain meaning later.
    assert_eq!(Direction::from_flags(1 << 3), None);
    assert_eq!(Direction::from_flags(u32::MAX), None);
    assert!(!Direction::ToDevice.device_writes());
    assert!(Direction::FromDevice.device_writes());
    assert!(Direction::Bidirectional.device_writes());
}

#[test]
fn performance_divider_matches_apex_driver() {
    assert_eq!(performance_clock_divider(0), Some(3));
    assert_eq!(performance_clock_divider(1), Some(2));
    assert_eq!(performance_clock_divider(2), Some(1));
    assert_eq!(performance_clock_divider(3), Some(0));
    assert_eq!(performance_clock_divider(4), None);
    assert_eq!(performance_clock_divider(u32::MAX), None);
}

#[test]
fn i386_encodings_match_gasket_headers_built_for_i386() {
    let g = golden_i386();
    let get = |n: &str| g.iter().find(|(k, _)| k == n).unwrap().1;
    assert_eq!(
        GASKET_IOCTL_CLEAR_EVENTFD_32 as u64,
        get("GASKET_IOCTL_CLEAR_EVENTFD")
    );
    assert_eq!(
        GASKET_IOCTL_LOOPBACK_INTERRUPT_32 as u64,
        get("GASKET_IOCTL_LOOPBACK_INTERRUPT")
    );
    assert_eq!(
        GASKET_IOCTL_MAP_BUFFER_FLAGS_32 as u64,
        get("GASKET_IOCTL_MAP_BUFFER_FLAGS")
    );
    assert_eq!(get("sizeof gasket_page_table_ioctl_flags"), 36);
    // Every other command is encoded identically for 32- and 64-bit callers.
    for (name, value) in golden() {
        if name.contains("CLEAR_EVENTFD")
            || name.contains("LOOPBACK")
            || name.contains("FLAGS")
            || name.starts_with("sizeof")
        {
            continue;
        }
        assert_eq!(get(&name), value, "{name}");
    }
}
