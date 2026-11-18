// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2018 Google, Inc.: derived from the gasket/apex driver.

//! Temperature sensor conversion and clock-throttling policy.
//!
//! Hardware-independent. The conversion constants are the linear fit from
//! apex_driver.c, which it describes as tuned for 25 °C to 100 °C.

/// Mask of the 10-bit ADC reading in OMC0_DC / OMC0_D4 / OMC0_D8.
pub(crate) const ADC_MASK: u32 = (1 << 10) - 1;
/// Bit position of the ADC field in those registers.
pub(crate) const ADC_SHIFT: u32 = 16;

/// Number of throttle trip points.
pub(crate) const NUM_TRIPS: usize = 3;

/// Default trip points in milli-degrees Celsius (apex_driver.c).
pub(crate) const DEFAULT_TRIPS_MILLIC: [i32; NUM_TRIPS] = [85_000, 90_000, 95_000];

/// Convert an ADC reading to milli-degrees Celsius.
pub(crate) const fn adc_to_millic(adc: u32) -> i32 {
    (662 - (adc & ADC_MASK) as i32) * 250 + 550
}

/// Convert milli-degrees Celsius to an ADC value, or `None` if the temperature
/// is outside what the 10-bit field can express. Uses C's truncating division so
/// values match the original driver bit for bit.
pub(crate) const fn millic_to_adc(millic: i32) -> Option<u32> {
    // Keep the arithmetic in range for any i32 input.
    let adc = (550i64 - millic as i64) / 250 + 662;
    if adc < 0 || adc > ADC_MASK as i64 {
        None
    } else {
        Some(adc as u32)
    }
}

/// Extract the current-temperature ADC field from an OMC0_DC value.
pub(crate) const fn adc_from_reg(reg: u32) -> u32 {
    (reg >> ADC_SHIFT) & ADC_MASK
}

/// Throttle trip points, stored as ADC values. A higher temperature is a *lower*
/// ADC value, so the stored values are descending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Trips {
    adc: [u32; NUM_TRIPS],
}

impl Trips {
    /// Build from ascending temperatures; `None` if out of range or unordered.
    pub(crate) fn from_millic(millic: [i32; NUM_TRIPS]) -> Option<Self> {
        let mut adc = [0; NUM_TRIPS];
        let mut i = 0;
        while i < NUM_TRIPS {
            adc[i] = millic_to_adc(millic[i])?;
            i += 1;
        }
        let trips = Self { adc };
        if trips.ordered() {
            Some(trips)
        } else {
            None
        }
    }

    pub(crate) fn defaults() -> Self {
        // The defaults are in range and ordered.
        match Self::from_millic(DEFAULT_TRIPS_MILLIC) {
            Some(t) => t,
            None => Self {
                adc: [0; NUM_TRIPS],
            },
        }
    }

    fn ordered(&self) -> bool {
        self.adc[0] >= self.adc[1] && self.adc[1] >= self.adc[2]
    }

    pub(crate) fn millic(&self, i: usize) -> i32 {
        adc_to_millic(self.adc[i])
    }

    /// Replace trip `i`, keeping the order. Returns `false` (and changes nothing)
    /// if the value is out of range or would break the order.
    pub(crate) fn set(&mut self, i: usize, millic: i32) -> bool {
        let Some(adc) = millic_to_adc(millic) else {
            return false;
        };
        if i >= NUM_TRIPS {
            return false;
        }
        let mut next = *self;
        next.adc[i] = adc;
        if !next.ordered() {
            return false;
        }
        *self = next;
        true
    }

    /// Throttle level for a temperature reading: 0 below trip 0, up to 3 at or
    /// above trip 2. The level is the GCB clock divider the thermal policy wants.
    pub(crate) fn level(&self, adc_temp: u32) -> u32 {
        let mut level = 0;
        for (i, &trip) in self.adc.iter().enumerate() {
            if adc_temp <= trip {
                level = i as u32 + 1;
            }
        }
        level
    }
}

/// How far below a trip point the chip must cool before throttling steps back
/// down. Without it the level flaps at every poll while the temperature sits on
/// a trip point (seen at 89–90 °C under sustained load; apex_driver.c flapped
/// the same way).
pub(crate) const HYSTERESIS_MILLIC: i32 = 2000;
/// [`HYSTERESIS_MILLIC`] in ADC counts (250 m°C per count).
const HYSTERESIS_ADC: u32 = (HYSTERESIS_MILLIC / 250) as u32;

impl Trips {
    /// Next throttle level from `current` for a reading: it rises immediately
    /// and falls past a trip only once the chip is [`HYSTERESIS_MILLIC`] below
    /// that trip.
    pub(crate) fn next_level(&self, current: u32, adc_temp: u32) -> u32 {
        let raw = self.level(adc_temp);
        if raw >= current {
            return raw;
        }
        let mut level = current.min(NUM_TRIPS as u32);
        while level > raw {
            // Colder is a higher ADC value.
            if adc_temp > self.adc[(level - 1) as usize].saturating_add(HYSTERESIS_ADC) {
                level -= 1;
            } else {
                break;
            }
        }
        level
    }
}

/// The divider actually programmed: the slower of what user space asked for and
/// what the temperature requires.
///
/// apex_driver.c let the thermal poll overwrite the divider every few seconds, so
/// a `PERFORMANCE_EXPECTATION` of LOW was silently undone while the chip was cool.
pub(crate) fn effective_divider(user: u32, thermal: u32) -> u32 {
    user.max(thermal).min(3)
}
