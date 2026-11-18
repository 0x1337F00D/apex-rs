// SPDX-License-Identifier: GPL-2.0-only

//! Parsing of sysfs input. Hardware-independent.

/// Parse a decimal integer the way `kstrtoint(buf, 10, ...)` accepts it: an
/// optional sign, digits and at most one trailing newline. Anything else,
/// including overflow, is rejected.
pub(crate) fn parse_i32(input: &[u8]) -> Option<i32> {
    let s = match input {
        [rest @ .., b'\n'] => rest,
        s => s,
    };
    let (neg, digits) = match s {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        s => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(c - b'0'))?;
        if value > i64::from(i32::MAX) + 1 {
            return None;
        }
    }
    let value = if neg { -value } else { value };
    i32::try_from(value).ok()
}
