// SPDX-License-Identifier: GPL-2.0-only

use crate::thermal::*;

/// The C expressions from apex_driver.c, evaluated with C int semantics.
fn c_adc_to_millic(adc: i32) -> i32 {
    (662 - adc) * 250 + 550
}
fn c_millic_to_adc(millic: i32) -> i32 {
    (550 - millic) / 250 + 662
}

#[test]
fn conversions_match_apex_driver() {
    for adc in 0..=ADC_MASK {
        assert_eq!(adc_to_millic(adc), c_adc_to_millic(adc as i32));
    }
    for millic in (-80_000..=160_000).step_by(37) {
        let c = c_millic_to_adc(millic);
        let ours = millic_to_adc(millic);
        if (0..=ADC_MASK as i32).contains(&c) {
            assert_eq!(ours, Some(c as u32), "{millic}");
        } else {
            assert_eq!(ours, None, "{millic}");
        }
    }
}

#[test]
fn conversions_reject_extremes_without_overflow() {
    assert_eq!(millic_to_adc(i32::MAX), None);
    assert_eq!(millic_to_adc(i32::MIN), None);
    // Only the 10-bit field is used.
    assert_eq!(adc_to_millic(0xffff_fc00 | 5), adc_to_millic(5));
    assert_eq!(adc_from_reg(0x03ff_0000), 0x3ff);
    assert_eq!(adc_from_reg(0xfc00_ffff), 0);
}

#[test]
fn default_trips_round_trip() {
    let t = Trips::defaults();
    for (i, expect) in DEFAULT_TRIPS_MILLIC.iter().enumerate() {
        // The 250 m°C quantisation may shift the value slightly, as in C.
        assert!(
            (t.millic(i) - expect).abs() < 250,
            "trip {i}: {}",
            t.millic(i)
        );
    }
}

#[test]
fn level_follows_trip_points() {
    let t = Trips::defaults();
    let adc = |m| millic_to_adc(m).unwrap();
    assert_eq!(t.level(adc(25_000)), 0);
    assert_eq!(t.level(adc(84_000)), 0);
    assert_eq!(t.level(adc(86_000)), 1);
    assert_eq!(t.level(adc(91_000)), 2);
    assert_eq!(t.level(adc(96_000)), 3);
    assert_eq!(t.level(0), 3); // hottest reading
    assert_eq!(t.level(ADC_MASK), 0); // coldest reading
}

#[test]
fn trips_keep_order_and_reject_bad_values() {
    let mut t = Trips::defaults();
    assert!(!t.set(0, 99_000)); // above trip 1
    assert!(!t.set(2, 80_000)); // below trip 1
    assert!(!t.set(1, 500_000)); // out of range
    assert!(!t.set(3, 90_000)); // no such trip
    assert_eq!(t, Trips::defaults());
    assert!(t.set(1, 88_000));
    assert!((t.millic(1) - 88_000).abs() < 250);
    assert!(Trips::from_millic([90_000, 85_000, 95_000]).is_none());
    assert!(Trips::from_millic([90_000, 90_000, 90_000]).is_some());
}

#[test]
fn effective_divider_never_speeds_up_past_either_request() {
    for user in 0..=3 {
        for thermal in 0..=3 {
            let d = effective_divider(user, thermal);
            assert!(d >= user && d >= thermal && d <= 3);
        }
    }
    // A cool chip keeps the user's LOW setting (gasket reset it to MAX).
    assert_eq!(effective_divider(3, 0), 3);
    assert_eq!(effective_divider(9, 9), 3);
}

#[test]
fn throttle_has_hysteresis() {
    let t = Trips::defaults();
    let adc = |m| millic_to_adc(m).unwrap();
    // Rising is immediate.
    assert_eq!(t.next_level(0, adc(90_100)), 2);
    // Oscillating around trip 1 (90 °C) no longer flaps.
    for m in [89_800, 89_300, 89_800, 88_500, 90_000] {
        assert_eq!(t.next_level(2, adc(m)), 2, "{m}");
    }
    // 2 °C below a trip it steps down past it.
    assert_eq!(t.next_level(2, adc(87_700)), 1);
    assert_eq!(t.next_level(1, adc(84_000)), 1);
    assert_eq!(t.next_level(1, adc(82_700)), 0);
    assert_eq!(t.next_level(3, adc(25_000)), 0);
    assert_eq!(t.next_level(0, adc(25_000)), 0);
}

#[test]
fn hysteresis_never_exceeds_raw_level_bounds() {
    let t = Trips::defaults();
    for current in 0..=5 {
        for a in 0..=ADC_MASK {
            let next = t.next_level(current, a);
            assert!(next <= 3 && next >= t.level(a), "current {current} adc {a}");
        }
    }
}
