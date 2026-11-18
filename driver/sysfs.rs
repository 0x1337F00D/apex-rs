// SPDX-License-Identifier: GPL-2.0-only

//! `/sys/class/apex/apex_N` and its attributes.
//!
//! libedgetpu finds devices by listing `/sys/class/apex`, and tools such as
//! Frigate read `temp` from there, so the class and the attribute names are
//! part of the ABI. The attribute set follows apex_driver.c and gasket_core.c;
//! `throttle` is new.

use crate::{
    device::Shared,
    sys::{self, device_attr, ClassDevice, SyncStatic},
    text,
};
use core::{
    fmt::{self, Write},
    ptr,
};
use kernel::{bindings, ffi::c_char, prelude::*};

/// Small formatting buffer for multi-value attributes.
struct Text {
    buf: [u8; 512],
    len: usize,
}

impl Text {
    fn new() -> Self {
        Self {
            buf: [0; 512],
            len: 0,
        }
    }
    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        let dst = self.buf.get_mut(self.len..end).ok_or(fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// What a show callback produces.
#[allow(
    clippy::large_enum_variant,
    reason = "built and consumed within one sysfs callback; boxing would add an allocation"
)]
enum Out {
    Int(i64),
    Text(Text),
}

fn errno(e: Error) -> isize {
    e.to_errno() as isize
}

/// # Safety
///
/// Called only as a show callback of an attribute in [`GROUPS`].
unsafe fn show(
    dev: *mut bindings::device,
    buf: *mut c_char,
    f: impl FnOnce(&Shared) -> Result<Out>,
) -> isize {
    // SAFETY: These attributes exist only on class devices created by
    // `ClassDevice::<Shared>::create()`, and this runs inside a sysfs callback.
    let shared = unsafe { ClassDevice::<Shared>::drvdata(dev) };
    match f(shared) {
        // SAFETY: `buf` is the sysfs page buffer.
        Ok(Out::Int(v)) => unsafe { sys::sysfs_emit_i64(buf, v) },
        // SAFETY: As above.
        Ok(Out::Text(t)) => unsafe { sys::sysfs_emit_bytes(buf, t.bytes()) },
        Err(e) => errno(e),
    }
}

/// # Safety
///
/// Called only as a store callback of an attribute in [`GROUPS`].
unsafe fn store(
    dev: *mut bindings::device,
    buf: *const c_char,
    count: usize,
    f: impl FnOnce(&Shared, i32) -> Result,
) -> isize {
    // SAFETY: As in `show()`.
    let shared = unsafe { ClassDevice::<Shared>::drvdata(dev) };
    // SAFETY: `buf`/`count` are the store callback's arguments.
    let input = unsafe { sys::sysfs_input(buf, count) };
    let Some(value) = text::parse_i32(input) else {
        return errno(EINVAL);
    };
    match f(shared, value) {
        Ok(()) => count as isize,
        Err(e) => errno(e),
    }
}

macro_rules! show_fn {
    ($name:ident, |$s:ident| $body:expr) => {
        unsafe extern "C" fn $name(
            dev: *mut bindings::device,
            _attr: *mut bindings::device_attribute,
            buf: *mut c_char,
        ) -> isize {
            // SAFETY: Registered as a show callback of an attribute in `GROUPS`.
            unsafe { show(dev, buf, |$s: &Shared| $body) }
        }
    };
}

macro_rules! store_fn {
    ($name:ident, |$s:ident, $v:ident| $body:expr) => {
        unsafe extern "C" fn $name(
            dev: *mut bindings::device,
            _attr: *mut bindings::device_attribute,
            buf: *const c_char,
            count: usize,
        ) -> isize {
            // SAFETY: Registered as a store callback of an attribute in `GROUPS`.
            unsafe { store(dev, buf, count, |$s: &Shared, $v: i32| $body) }
        }
    };
}

show_fn!(temp_show, |s| Ok(Out::Int(i64::from(
    s.temperature_millic()?
))));
show_fn!(trip0_show, |s| Ok(Out::Int(i64::from(s.trip(0)))));
show_fn!(trip1_show, |s| Ok(Out::Int(i64::from(s.trip(1)))));
show_fn!(trip2_show, |s| Ok(Out::Int(i64::from(s.trip(2)))));
store_fn!(trip0_store, |s, v| s.set_trip(0, v));
store_fn!(trip1_store, |s, v| s.set_trip(1, v));
store_fn!(trip2_store, |s, v| s.set_trip(2, v));
show_fn!(warn1_show, |s| Ok(Out::Int(i64::from(s.hw_warn(1).0))));
show_fn!(warn2_show, |s| Ok(Out::Int(i64::from(s.hw_warn(2).0))));
show_fn!(warn1_en_show, |s| Ok(Out::Int(i64::from(s.hw_warn(1).1))));
show_fn!(warn2_en_show, |s| Ok(Out::Int(i64::from(s.hw_warn(2).1))));
store_fn!(warn1_store, |s, v| s.set_hw_warn(1, Some(v), None));
store_fn!(warn2_store, |s, v| s.set_hw_warn(2, Some(v), None));
store_fn!(warn1_en_store, |s, v| s.set_hw_warn(1, None, Some(v > 0)));
store_fn!(warn2_en_store, |s, v| s.set_hw_warn(2, None, Some(v > 0)));
show_fn!(poll_show, |s| Ok(Out::Int(i64::from(
    s.temp_poll_interval()
))));

unsafe extern "C" fn poll_store(
    dev: *mut bindings::device,
    _attr: *mut bindings::device_attribute,
    buf: *const c_char,
    count: usize,
) -> isize {
    // SAFETY: Registered as a store callback of an attribute in `GROUPS`; the
    // driver data is an `Arc<Shared>` reference held by the class device.
    let shared = unsafe { ClassDevice::<Shared>::drvdata_arc(dev) };
    // SAFETY: `buf`/`count` are the store callback's arguments.
    let input = unsafe { sys::sysfs_input(buf, count) };
    match text::parse_i32(input) {
        Some(ms) if ms >= 0 => {
            shared.set_temp_poll_interval(ms as u32);
            count as isize
        }
        _ => errno(EINVAL),
    }
}

show_fn!(throttle_show, |s| {
    let (level, effective, programmed) = s.throttle();
    let mut t = Text::new();
    writeln!(
        t,
        "level {level} divider {effective} programmed {programmed}"
    )
    .map_err(|_| EINVAL)?;
    Ok(Out::Text(t))
});
show_fn!(unique_id_show, |s| {
    let id = s.unique_id()?;
    let mut t = Text::new();
    writeln!(t, "{:08x}{:08x}{:08x}{:08x}", id[0], id[1], id[2], id[3]).map_err(|_| EINVAL)?;
    Ok(Out::Text(t))
});
show_fn!(pt_entries_show, |s| Ok(Out::Int(
    s.page_table_info().0 as i64
)));
show_fn!(pt_simple_show, |s| Ok(Out::Int(
    s.page_table_info().1 as i64
)));
show_fn!(pt_mapped_show, |s| Ok(Out::Int(
    s.page_table_info().2 as i64
)));
show_fn!(irq_counts_show, |s| {
    let mut t = Text::new();
    for i in 0..crate::regs::NUM_INTERRUPTS {
        writeln!(t, "0x{:02x}: {}", i, s.interrupt_count(i)).map_err(|_| EINVAL)?;
    }
    Ok(Out::Text(t))
});
show_fn!(status_show, |s| {
    let mut t = Text::new();
    t.write_str(if s.ownership().0 { "ALIVE\n" } else { "DEAD\n" })
        .map_err(|_| EINVAL)?;
    Ok(Out::Text(t))
});
show_fn!(is_owned_show, |s| Ok(Out::Int(i64::from(
    s.ownership().1 != 0
))));
show_fn!(owner_show, |s| Ok(Out::Int(i64::from(s.ownership().1))));
show_fn!(write_open_count_show, |s| Ok(Out::Int(i64::from(
    s.ownership().2
))));
show_fn!(reset_count_show, |s| Ok(Out::Int(s.ownership().3 as i64)));
show_fn!(driver_version_show, |_s| {
    let mut t = Text::new();
    writeln!(t, "{}", crate::DRIVER_VERSION).map_err(|_| EINVAL)?;
    Ok(Out::Text(t))
});
show_fn!(framework_version_show, |_s| {
    let mut t = Text::new();
    // The gasket framework version whose ABI this driver implements.
    t.write_str("1.1.4\n").map_err(|_| EINVAL)?;
    Ok(Out::Text(t))
});

const RO: u16 = 0o444;
const RW: u16 = 0o644;

const NUM_ATTRS: usize = 22;

static ATTRS: [SyncStatic<bindings::device_attribute>; NUM_ATTRS] = [
    SyncStatic(device_attr(c"temp", RO, temp_show, None)),
    SyncStatic(device_attr(
        c"trip_point0_temp",
        RW,
        trip0_show,
        Some(trip0_store),
    )),
    SyncStatic(device_attr(
        c"trip_point1_temp",
        RW,
        trip1_show,
        Some(trip1_store),
    )),
    SyncStatic(device_attr(
        c"trip_point2_temp",
        RW,
        trip2_show,
        Some(trip2_store),
    )),
    SyncStatic(device_attr(
        c"hw_temp_warn1",
        RW,
        warn1_show,
        Some(warn1_store),
    )),
    SyncStatic(device_attr(
        c"hw_temp_warn1_en",
        RW,
        warn1_en_show,
        Some(warn1_en_store),
    )),
    SyncStatic(device_attr(
        c"hw_temp_warn2",
        RW,
        warn2_show,
        Some(warn2_store),
    )),
    SyncStatic(device_attr(
        c"hw_temp_warn2_en",
        RW,
        warn2_en_show,
        Some(warn2_en_store),
    )),
    SyncStatic(device_attr(
        c"temp_poll_interval",
        RW,
        poll_show,
        Some(poll_store),
    )),
    SyncStatic(device_attr(c"throttle", RO, throttle_show, None)),
    SyncStatic(device_attr(c"unique_id", RO, unique_id_show, None)),
    SyncStatic(device_attr(
        c"node_0_page_table_entries",
        RO,
        pt_entries_show,
        None,
    )),
    SyncStatic(device_attr(
        c"node_0_simple_page_table_entries",
        RO,
        pt_simple_show,
        None,
    )),
    SyncStatic(device_attr(
        c"node_0_num_mapped_pages",
        RO,
        pt_mapped_show,
        None,
    )),
    SyncStatic(device_attr(c"interrupt_counts", RO, irq_counts_show, None)),
    SyncStatic(device_attr(c"status", RO, status_show, None)),
    SyncStatic(device_attr(c"is_device_owned", RO, is_owned_show, None)),
    SyncStatic(device_attr(c"device_owner", RO, owner_show, None)),
    SyncStatic(device_attr(
        c"write_open_count",
        RO,
        write_open_count_show,
        None,
    )),
    SyncStatic(device_attr(c"reset_count", RO, reset_count_show, None)),
    SyncStatic(device_attr(
        c"driver_version",
        RO,
        driver_version_show,
        None,
    )),
    SyncStatic(device_attr(
        c"framework_version",
        RO,
        framework_version_show,
        None,
    )),
];

/// NULL-terminated `struct attribute *` array for the group.
static ATTR_PTRS: SyncStatic<[*mut bindings::attribute; NUM_ATTRS + 1]> = SyncStatic({
    let mut a = [ptr::null_mut(); NUM_ATTRS + 1];
    let mut i = 0;
    while i < NUM_ATTRS {
        a[i] = (&raw const ATTRS[i].0.attr).cast_mut();
        i += 1;
    }
    a
});

static GROUP: SyncStatic<bindings::attribute_group> = SyncStatic({
    let mut g: bindings::attribute_group = pin_init::zeroed();
    g.__bindgen_anon_2.attrs = (&raw const ATTR_PTRS.0).cast_mut().cast();
    g
});

static GROUPS: SyncStatic<[*const bindings::attribute_group; 2]> =
    SyncStatic([&raw const GROUP.0, ptr::null()]);

/// The `apex` device class. Registered once per module load.
pub(crate) static CLASS: SyncStatic<bindings::class> = SyncStatic({
    let mut c: bindings::class = pin_init::zeroed();
    c.name = c"apex".as_ptr().cast();
    c.dev_groups = (&raw const GROUPS.0).cast();
    c
});

// hwmon: `sensors` and other monitoring tools read these.
show_fn!(hwmon_temp_show, |s| Ok(Out::Int(i64::from(
    s.temperature_millic()?
))));
show_fn!(hwmon_max_show, |s| Ok(Out::Int(i64::from(s.trip(0)))));
show_fn!(hwmon_crit_show, |s| Ok(Out::Int(i64::from(s.trip(2)))));

static HWMON_ATTRS: [SyncStatic<bindings::device_attribute>; 3] = [
    SyncStatic(device_attr(c"temp1_input", RO, hwmon_temp_show, None)),
    SyncStatic(device_attr(c"temp1_max", RO, hwmon_max_show, None)),
    SyncStatic(device_attr(c"temp1_crit", RO, hwmon_crit_show, None)),
];

static HWMON_ATTR_PTRS: SyncStatic<[*mut bindings::attribute; 4]> = SyncStatic([
    (&raw const HWMON_ATTRS[0].0.attr).cast_mut(),
    (&raw const HWMON_ATTRS[1].0.attr).cast_mut(),
    (&raw const HWMON_ATTRS[2].0.attr).cast_mut(),
    ptr::null_mut(),
]);

static HWMON_GROUP: SyncStatic<bindings::attribute_group> = SyncStatic({
    let mut g: bindings::attribute_group = pin_init::zeroed();
    g.__bindgen_anon_2.attrs = (&raw const HWMON_ATTR_PTRS.0).cast_mut().cast();
    g
});

/// Attribute groups of the `apex` hwmon device.
pub(crate) static HWMON_GROUPS: SyncStatic<[*const bindings::attribute_group; 2]> =
    SyncStatic([&raw const HWMON_GROUP.0, ptr::null()]);
