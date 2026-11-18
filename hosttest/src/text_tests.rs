// SPDX-License-Identifier: GPL-2.0-only

use crate::text::parse_i32;

#[test]
fn accepts_what_kstrtoint_accepts() {
    assert_eq!(parse_i32(b"0"), Some(0));
    assert_eq!(parse_i32(b"85000\n"), Some(85000));
    assert_eq!(parse_i32(b"+7"), Some(7));
    assert_eq!(parse_i32(b"-40000"), Some(-40000));
    assert_eq!(parse_i32(b"2147483647"), Some(i32::MAX));
    assert_eq!(parse_i32(b"-2147483648\n"), Some(i32::MIN));
}

#[test]
fn rejects_malformed_and_overflowing_input() {
    for bad in [
        &b""[..],
        b"\n",
        b"-",
        b"+\n",
        b"12a",
        b" 12",
        b"12 ",
        b"12\n\n",
        b"0x10",
        b"2147483648",
        b"-2147483649",
        b"99999999999999999999999",
        b"1\x00",
    ] {
        assert_eq!(parse_i32(bad), None, "{:?}", String::from_utf8_lossy(bad));
    }
}
