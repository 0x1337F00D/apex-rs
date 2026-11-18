// SPDX-License-Identifier: GPL-2.0-only

//! Safe wrappers for kernel C APIs that the Rust `kernel` crate does not cover
//! yet: eventfd, long-term user page pinning, scatter-gather DMA of those pages,
//! device classes, `pci_mmap_resource_range()`, `dma_mmap_attrs()` and
//! `unmap_mapping_range()`.
//!
//! Raw FFI is confined to this file so the rest of the driver uses only safe
//! interfaces. Each wrapper owns exactly one kernel resource and releases it in
//! `Drop`.

use core::{
    cell::UnsafeCell,
    ffi::{c_int, c_uint, c_void},
    ptr::{self, NonNull},
};
use kernel::{
    bindings,
    device::{self, Bound},
    dma,
    error::{from_err_ptr, to_result},
    ffi::c_char,
    io::Io,
    mm::virt::VmaNew,
    pci,
    prelude::*,
    scatterlist::SGTable,
    sync::aref::ARef,
    sync::{
        atomic::{ordering, Atomic},
        rcu, Arc, ArcBorrow,
    },
};

/// The `struct device` behind a Rust device reference.
fn dev_raw<Ctx: device::DeviceContext>(dev: &device::Device<Ctx>) -> *mut bindings::device {
    // `device::Device` is a transparent wrapper around `struct device`.
    core::ptr::from_ref(dev).cast_mut().cast()
}

/// Page size as seen by the device and by this driver.
pub(crate) const PAGE: usize = 4096;
const _: () = assert!(
    bindings::PAGE_SIZE == PAGE,
    "the driver assumes 4 KiB pages"
);

/// A `static` holding C structures that contain raw pointers. They are never
/// written after initialisation, so sharing them between threads is fine.
#[repr(transparent)]
pub(crate) struct SyncStatic<T>(pub(crate) T);
// SAFETY: Only used for immutable C descriptor tables (class, attributes, vm ops)
// that are fully initialised at compile time and never mutated.
unsafe impl<T> Sync for SyncStatic<T> {}

// -------------------------------------------------------------------------------
// eventfd
// -------------------------------------------------------------------------------

/// A counted reference to an eventfd context.
pub(crate) struct EventFd(NonNull<bindings::eventfd_ctx>);

// SAFETY: eventfd contexts are reference counted and thread safe.
unsafe impl Send for EventFd {}
// SAFETY: See above.
unsafe impl Sync for EventFd {}

impl EventFd {
    /// Take a reference to the eventfd behind file descriptor `fd` of the caller.
    pub(crate) fn from_fd(fd: u64) -> Result<Self> {
        let fd = c_int::try_from(fd).map_err(|_| EBADF)?;
        // SAFETY: `eventfd_ctx_fdget()` validates `fd` and returns a counted
        // reference or an error pointer.
        let ctx = from_err_ptr(unsafe { bindings::eventfd_ctx_fdget(fd) })?;
        NonNull::new(ctx).map(Self).ok_or(EBADF)
    }

    fn into_raw(self) -> *mut bindings::eventfd_ctx {
        let p = self.0.as_ptr();
        core::mem::forget(self);
        p
    }

    /// # Safety
    ///
    /// `ptr` must come from [`EventFd::into_raw`] and not be used again.
    unsafe fn from_raw(ptr: *mut bindings::eventfd_ctx) -> Option<Self> {
        NonNull::new(ptr).map(Self)
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        // SAFETY: We own one reference, taken by `eventfd_ctx_fdget()`.
        unsafe { bindings::eventfd_ctx_put(self.0.as_ptr()) };
    }
}

/// An eventfd slot that interrupt handlers signal without taking locks.
///
/// Writers swap the pointer and wait for an RCU grace period before dropping the
/// old reference. Hard-IRQ handlers are RCU readers, so after
/// `synchronize_rcu()` no handler can still be using the old context.
pub(crate) struct EventSlot(Atomic<*mut bindings::eventfd_ctx>);

impl EventSlot {
    pub(crate) fn new() -> Self {
        Self(Atomic::new(ptr::null_mut()))
    }

    /// Install `new` (or clear the slot) and release the previous eventfd.
    /// May sleep.
    pub(crate) fn replace(&self, new: Option<EventFd>) {
        let new = new.map_or(ptr::null_mut(), EventFd::into_raw);
        let old = self.0.xchg(new, ordering::Full);
        if !old.is_null() {
            rcu::synchronize_rcu();
            // SAFETY: `old` was stored by `replace()` from `EventFd::into_raw()`,
            // and after the grace period no reader holds it.
            drop(unsafe { EventFd::from_raw(old) });
        }
    }

    /// Signal the installed eventfd, if any. Callable from hard-IRQ context.
    pub(crate) fn signal(&self) {
        let _guard = rcu::read_lock();
        let ctx = self.0.load(ordering::Acquire);
        if !ctx.is_null() {
            // SAFETY: `ctx` stays valid until a grace period after it is removed,
            // and we are inside an RCU read-side critical section.
            unsafe { bindings::eventfd_signal_mask(ctx, 0) };
        }
    }
}

impl Drop for EventSlot {
    fn drop(&mut self) {
        // Interrupt handlers are gone by the time the slots are dropped (they hold
        // a reference to the containing object), so no grace period is needed.
        let old = self.0.xchg(ptr::null_mut(), ordering::Relaxed);
        // SAFETY: As in `replace()`.
        drop(unsafe { EventFd::from_raw(old) });
    }
}

// -------------------------------------------------------------------------------
// Long-term pinned user pages
// -------------------------------------------------------------------------------

/// User pages pinned with `FOLL_LONGTERM` for device DMA.
///
/// gasket used `get_user_pages_fast()` one page at a time. That does not keep the
/// kernel from migrating the pages (CMA, ZONE_MOVABLE, compaction) while the
/// device is still writing to them; `FOLL_LONGTERM` pins do.
pub(crate) struct PinnedPages {
    pages: KVVec<*mut bindings::page>,
    dirty: bool,
}

// SAFETY: The pinned pages are not tied to any thread.
unsafe impl Send for PinnedPages {}
// SAFETY: `PinnedPages` has no interior mutability.
unsafe impl Sync for PinnedPages {}

impl PinnedPages {
    /// Pin `count` pages of the calling process starting at the page-aligned
    /// address `addr`. Must be called without driver locks held: it may take the
    /// caller's `mmap_lock`.
    pub(crate) fn pin(addr: u64, count: usize, writable: bool) -> Result<Self> {
        if !addr.is_multiple_of(PAGE as u64) || count == 0 {
            return Err(EINVAL);
        }
        let mut pages = KVVec::<*mut bindings::page>::with_capacity(count, GFP_KERNEL)?;
        let mut flags = bindings::FOLL_LONGTERM;
        if writable {
            flags |= bindings::FOLL_WRITE;
        }
        let mut this = Self {
            pages: KVVec::new(),
            dirty: writable,
        };
        while pages.len() < count {
            let done = pages.len();
            let start = addr.checked_add((done * PAGE) as u64).ok_or(EFAULT)?;
            let want = c_int::try_from(count - done).unwrap_or(c_int::MAX);
            // SAFETY: `pages` has capacity for `count` entries and `done` are
            // initialised, so the next `want` slots are writable. The function
            // validates the user range itself.
            let got = unsafe {
                bindings::pin_user_pages_fast(
                    start as kernel::ffi::c_ulong,
                    want,
                    flags as c_uint,
                    pages.as_mut_ptr().add(done),
                )
            };
            if got <= 0 {
                this.pages = pages;
                // `this` unpins what was pinned so far when it drops.
                return Err(if got == 0 {
                    EFAULT
                } else {
                    Error::from_errno(got as c_int)
                });
            }
            // SAFETY: `pin_user_pages_fast()` initialised `got` more entries.
            unsafe { pages.inc_len(got as usize) };
        }
        this.pages = pages;
        Ok(this)
    }

    pub(crate) fn len(&self) -> usize {
        self.pages.len()
    }
}

impl Drop for PinnedPages {
    fn drop(&mut self) {
        if self.pages.is_empty() {
            return;
        }
        // SAFETY: Every entry is a page we pinned with `pin_user_pages_fast()`
        // and has not been unpinned.
        unsafe {
            bindings::unpin_user_pages_dirty_lock(
                self.pages.as_mut_ptr(),
                self.pages.len() as kernel::ffi::c_ulong,
                self.dirty,
            )
        };
    }
}

// -------------------------------------------------------------------------------
// Scatter-gather DMA mapping of pinned pages
// -------------------------------------------------------------------------------

/// A scatter-gather table over [`PinnedPages`], mapped for DMA.
///
/// One `dma_map_sgtable()` per buffer lets the IOMMU map it as a few large
/// contiguous IOVA ranges, instead of gasket's one `dma_map_page()` per page.
pub(crate) struct DmaSgt {
    sgt: KBox<UnsafeCell<bindings::sg_table>>,
    dev: ARef<device::Device>,
    dir: dma::DataDirection,
    mapped: bool,
}

// SAFETY: The table is only accessed through `&self`/`&mut self`.
unsafe impl Send for DmaSgt {}
// SAFETY: Shared access only reads the table.
unsafe impl Sync for DmaSgt {}

impl DmaSgt {
    /// Map `pages` for DMA by `dev`. The caller keeps `pages` alive (and pinned)
    /// for as long as the returned mapping exists.
    pub(crate) fn new(
        dev: &device::Device<Bound>,
        pages: &mut PinnedPages,
        dir: dma::DataDirection,
    ) -> Result<Self> {
        let count = c_uint::try_from(pages.len()).map_err(|_| EINVAL)?;
        // SAFETY: An all-zero `sg_table` is its documented empty state.
        let sgt = KBox::new(UnsafeCell::new(unsafe { core::mem::zeroed() }), GFP_KERNEL)?;
        let mut this = Self {
            sgt,
            dev: dev.into(),
            dir,
            mapped: false,
        };
        // Respect both the device's segment limit and the DMA layer's mapping
        // limit (e.g. swiotlb bounce buffers).
        // SAFETY: `dev` is a valid, bound device.
        let max_mapping = match unsafe { bindings::dma_max_mapping_size(dev_raw(dev)) } {
            0 => c_uint::MAX,
            n => c_uint::try_from(n).unwrap_or(c_uint::MAX),
        };
        // `dma_get_max_seg_size()` is inline: the limit is `dma_parms`, 64 KiB
        // without it.
        // SAFETY: `dma_parms` of a bound device is either NULL or valid.
        let max_seg = unsafe {
            let parms = (*dev_raw(dev)).dma_parms;
            if parms.is_null() {
                65536
            } else {
                (*parms).max_segment_size
            }
        };
        let max_segment = max_mapping.min(max_seg).max(PAGE as c_uint) & !(PAGE as c_uint - 1);
        // SAFETY: `sgt` is a zeroed table, `pages` holds `count` valid pinned
        // pages that stay pinned while `this` lives (caller contract).
        to_result(unsafe {
            bindings::sg_alloc_table_from_pages_segment(
                this.sgt.get(),
                pages.pages.as_mut_ptr(),
                count,
                0,
                count as usize * PAGE,
                max_segment,
                bindings::GFP_KERNEL,
            )
        })?;
        // SAFETY: The table was just built for pages that `dev` may access.
        to_result(unsafe {
            bindings::dma_map_sgtable(dev_raw(dev), this.sgt.get(), dir.into(), 0)
        })?;
        this.mapped = true;
        Ok(this)
    }

    /// The DMA address of every 4 KiB page, in buffer order.
    pub(crate) fn dma_pages(&self) -> impl Iterator<Item = u64> + '_ {
        // SAFETY: The table is valid and not modified while `self` is borrowed.
        let table = unsafe { SGTable::from_raw(self.sgt.get()) };
        table.iter().flat_map(|sg| {
            let base = sg.dma_address();
            let len = sg.dma_len();
            (0..len / PAGE as u64).map(move |i| base + i * PAGE as u64)
        })
    }
}

impl Drop for DmaSgt {
    fn drop(&mut self) {
        if self.mapped {
            // SAFETY: The table is mapped for `dev` with `dir`. The driver drops
            // mappings only while the device is bound (see `device.rs`).
            unsafe {
                bindings::dma_unmap_sgtable(dev_raw(&self.dev), self.sgt.get(), self.dir.into(), 0)
            };
        }
        // SAFETY: `sg_free_table()` accepts both an allocated and a zeroed table.
        unsafe { bindings::sg_free_table(self.sgt.get()) };
    }
}

// -------------------------------------------------------------------------------
// The "apex" device class
// -------------------------------------------------------------------------------

/// Show callback: formats into `buf` (one page) and returns its length.
pub(crate) type ShowFn = unsafe extern "C" fn(
    *mut bindings::device,
    *mut bindings::device_attribute,
    *mut c_char,
) -> isize;
/// Store callback.
pub(crate) type StoreFn = unsafe extern "C" fn(
    *mut bindings::device,
    *mut bindings::device_attribute,
    *const c_char,
    usize,
) -> isize;

/// Build a `struct device_attribute` at compile time.
pub(crate) const fn device_attr(
    name: &'static CStr,
    mode: u16,
    show: ShowFn,
    store: Option<StoreFn>,
) -> bindings::device_attribute {
    let mut a: bindings::device_attribute = pin_init::zeroed();
    a.attr.name = name.as_ptr().cast();
    a.attr.mode = mode;
    a.__bindgen_anon_1.show = Some(show);
    a.__bindgen_anon_2.store = store;
    a
}

/// Registration of a device class. `class` must be a `'static` descriptor.
pub(crate) struct ClassRegistration(&'static SyncStatic<bindings::class>);

impl ClassRegistration {
    pub(crate) fn register(class: &'static SyncStatic<bindings::class>) -> Result<Self> {
        // SAFETY: `class` is a valid, static class descriptor.
        to_result(unsafe { bindings::class_register(&class.0) })?;
        Ok(Self(class))
    }
}

impl Drop for ClassRegistration {
    fn drop(&mut self) {
        // SAFETY: Registered in `register()`.
        unsafe { bindings::class_unregister(&self.0 .0) };
    }
}

/// A class device without a device number: it exists to give user space a
/// `/sys/class/<class>/<name>` entry with attributes and a link to the parent.
///
/// `drvdata` is an `Arc<T>` reference that the device holds until it is removed.
pub(crate) struct ClassDevice<T: Send + Sync> {
    dev: NonNull<bindings::device>,
    drvdata: *const T,
}

// SAFETY: The device pointer is only used to unregister it, which may happen on
// any thread; `T` is `Send + Sync`.
unsafe impl<T: Send + Sync> Send for ClassDevice<T> {}
// SAFETY: No shared-reference operations.
unsafe impl<T: Send + Sync> Sync for ClassDevice<T> {}

impl<T: Send + Sync> ClassDevice<T> {
    /// Create `/sys/class/<class>/<prefix><index>` below `parent`.
    /// The class must stay registered while the device exists (the module
    /// unregisters its PCI driver, and with it every device, first).
    pub(crate) fn create(
        class: &'static SyncStatic<bindings::class>,
        parent: &device::Device<Bound>,
        fmt: &'static CStr,
        index: u32,
        drvdata: Arc<T>,
    ) -> Result<Self> {
        let data = Arc::into_raw(drvdata);
        // SAFETY: `class` is registered, `parent` is a live device, `fmt` takes
        // exactly one `%u` argument, which we pass.
        let dev = unsafe {
            bindings::device_create(
                &class.0,
                dev_raw(parent),
                0,
                data.cast_mut().cast::<c_void>(),
                fmt.as_char_ptr(),
                index as c_uint,
            )
        };
        match from_err_ptr(dev).ok().and_then(NonNull::new) {
            Some(dev) => Ok(Self { dev, drvdata: data }),
            None => {
                // SAFETY: Reclaim the reference handed out above.
                drop(unsafe { Arc::from_raw(data) });
                Err(from_err_ptr(dev).err().unwrap_or(ENOMEM))
            }
        }
    }

    /// The driver data of a class device created by [`ClassDevice::create`].
    ///
    /// # Safety
    ///
    /// `dev` must be a device created by `ClassDevice::<T>::create()`, and the
    /// call must come from one of its sysfs callbacks (which `device_unregister()`
    /// waits for before the reference is released).
    pub(crate) unsafe fn drvdata<'a>(dev: *mut bindings::device) -> &'a T {
        // SAFETY: Per the contract, `driver_data` holds a live `Arc<T>` pointer.
        unsafe { &*(*dev).driver_data.cast::<T>() }
    }
}

impl<T: Send + Sync> ClassDevice<T> {
    /// Like [`ClassDevice::drvdata`], but returns a new `Arc<T>` reference.
    ///
    /// # Safety
    ///
    /// As for [`ClassDevice::drvdata`].
    pub(crate) unsafe fn drvdata_arc(dev: *mut bindings::device) -> Arc<T> {
        // SAFETY: Per the contract, `driver_data` is a live `Arc<T>` pointer.
        unsafe { ArcBorrow::from_raw((*dev).driver_data.cast::<T>().cast_const()) }.into()
    }
}

impl<T: Send + Sync> Drop for ClassDevice<T> {
    fn drop(&mut self) {
        // SAFETY: We created the device; this removes it and waits for running
        // sysfs callbacks to finish.
        unsafe { bindings::device_unregister(self.dev.as_ptr()) };
        // SAFETY: No callback can observe `drvdata` any more.
        drop(unsafe { Arc::from_raw(self.drvdata) });
    }
}

/// Emit `value` followed by a newline into a sysfs buffer.
///
/// # Safety
///
/// `buf` must be the page-sized buffer passed to a sysfs show callback.
pub(crate) unsafe fn sysfs_emit_i64(buf: *mut c_char, value: i64) -> isize {
    // SAFETY: Per the contract; the format consumes one `long long`.
    unsafe {
        bindings::sysfs_emit(buf, c"%lld\n".as_char_ptr(), value as core::ffi::c_longlong) as isize
    }
}

/// Emit a string into a sysfs buffer.
///
/// # Safety
///
/// As for [`sysfs_emit_i64`]; `s` is copied verbatim.
pub(crate) unsafe fn sysfs_emit_bytes(buf: *mut c_char, s: &[u8]) -> isize {
    let n = s.len().min(PAGE - 1);
    // SAFETY: `buf` is one page long and `n < PAGE`.
    unsafe {
        ptr::copy_nonoverlapping(s.as_ptr(), buf.cast::<u8>(), n);
        *buf.add(n) = 0;
    }
    n as isize
}

/// The bytes written to a sysfs store callback.
///
/// # Safety
///
/// `buf` and `count` must be the arguments of a sysfs store callback.
pub(crate) unsafe fn sysfs_input<'a>(buf: *const c_char, count: usize) -> &'a [u8] {
    // SAFETY: sysfs passes a buffer of `count` readable bytes.
    unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), count) }
}

// -------------------------------------------------------------------------------
// mmap helpers
// -------------------------------------------------------------------------------

/// The file page offset of a VMA being created.
pub(crate) fn vma_pgoff(vma: &VmaNew) -> u64 {
    // SAFETY: `vm_pgoff` of a VMA under construction is stable while mmap holds
    // the `mmap_lock` for writing.
    unsafe { (*vma.as_ptr()).vm_pgoff as u64 }
}

extern "C" {
    /// `helpers.c`: `vma->vm_page_prot = pgprot_device(vma->vm_page_prot)`.
    // Declared with `c_void` because the opaque `vm_area_struct` binding is not
    // FFI-safe by value; only the pointer crosses the boundary.
    fn apex_rs_vma_set_device_prot(vma: *mut c_void);
}

/// Map BAR `bar` of `pdev` into `vma` with device (uncached) memory attributes.
/// `vm_pgoff` is the page offset into the BAR and is left unchanged, so the
/// mapping can later be found by file offset.
pub(crate) fn mmap_bar(pdev: &pci::Device<Bound>, bar: u32, vma: &VmaNew) -> Result {
    let start = pdev.resource_start(bar)?;
    let len = pdev.resource_len(bar)?;
    // SAFETY: `vma` is being set up by mmap; changing its protection is allowed.
    unsafe { apex_rs_vma_set_device_prot(vma.as_ptr().cast()) };
    // SAFETY: `[start, start + len)` is BAR `bar` of a bound device, and the
    // caller checked the requested range against one of its mappable windows.
    // `vm_iomap_memory()` checks the VMA against the range.
    to_result(unsafe {
        bindings::vm_iomap_memory(vma.as_ptr(), start, len as kernel::ffi::c_ulong)
    })?;
    // SAFETY: The VMA is under construction. Without `vm_ops` it would count as
    // anonymous memory; with ops but no `fault` handler, an access after the
    // mapping is revoked raises SIGBUS. `access` lets debuggers read registers.
    unsafe { (*vma.as_ptr()).vm_ops = &BAR_VM_OPS.0 };
    Ok(())
}

/// `vm_operations_struct` of register mappings, like the PCI core's
/// `pci_phys_vm_ops`.
static BAR_VM_OPS: SyncStatic<bindings::vm_operations_struct> = SyncStatic({
    let mut ops: bindings::vm_operations_struct = pin_init::zeroed();
    ops.access = Some(bindings::generic_access_phys);
    ops
});

fn pci_dev_raw(pdev: &pci::Device<Bound>) -> *mut bindings::pci_dev {
    // `pci::Device` is a transparent wrapper around `struct pci_dev`.
    core::ptr::from_ref(pdev).cast_mut().cast()
}

/// Re-arm MSI-X after the chip re-initialised its MSI-X table (it does so after
/// losing power, e.g. in D3hot).
///
/// On bare metal, `pci_restore_msi_state()` rewrites the table from the PCI
/// core's cached messages. Under VFIO passthrough the guest's table writes only
/// reach QEMU's emulated copy, so the hardware table would stay blank and no
/// interrupt would ever arrive. Clearing and setting MSI-X Enable first makes
/// the hypervisor tear down and re-establish the vectors, which rewrites the
/// hardware table. gasket achieved the same by freeing and re-requesting all
/// vectors after every reset. The device is idle while this runs.
pub(crate) fn rearm_msix(pdev: &pci::Device<Bound>) {
    const ENABLE: u16 = 1 << 15;
    // SAFETY: `pdev` is valid; `msix_cap` is set by the PCI core at enumeration.
    let cap = unsafe { (*pci_dev_raw(pdev)).msix_cap } as usize;
    if cap != 0 {
        let cfg = pdev.config_space();
        let ctrl_off = cap + 2; // PCI_MSIX_FLAGS
        if let Ok(ctrl) = cfg.try_read16(ctrl_off) {
            if ctrl & ENABLE != 0 {
                let _ = cfg.try_write16(ctrl & !ENABLE, ctrl_off);
                let _ = cfg.try_write16(ctrl, ctrl_off);
            }
        }
    }
    // SAFETY: `pdev` is bound and has MSI-X enabled by this driver.
    unsafe { bindings::pci_restore_msi_state(pci_dev_raw(pdev)) };
}

/// Map a DMA-coherent buffer into `vma`, starting `offset` bytes into it.
pub(crate) fn mmap_coherent(
    dev: &device::Device<Bound>,
    vma: &VmaNew,
    buf: &dma::Coherent<[u8]>,
    offset: u64,
) -> Result {
    let pgoff = vma_pgoff(vma);
    let len = (vma.end() - vma.start()) as u64;
    if !offset.is_multiple_of(PAGE as u64)
        || offset.checked_add(len).ok_or(EINVAL)? > buf.size() as u64
    {
        return Err(EINVAL);
    }
    // SAFETY: `dma_mmap_attrs()` interprets `vm_pgoff` as the page offset into the
    // buffer; we set it for the call and restore the file offset afterwards.
    unsafe { (*vma.as_ptr()).vm_pgoff = (offset / PAGE as u64) as kernel::ffi::c_ulong };
    // SAFETY: `buf` was allocated for `dev` with no attributes and is at least
    // `offset + len` bytes long.
    let ret = unsafe {
        bindings::dma_mmap_attrs(
            dev_raw(dev),
            vma.as_ptr(),
            buf.as_ptr().cast::<c_void>().cast_mut(),
            buf.dma_address(),
            buf.size(),
            0,
        )
    };
    // SAFETY: As above.
    unsafe { (*vma.as_ptr()).vm_pgoff = pgoff as kernel::ffi::c_ulong };
    to_result(ret)
}

/// Attach an `Arc<T>` reference to `vma`, released when the last VMA derived
/// from it is unmapped. `ops` must have been built with [`vm_ops_for`]`::<T>()`.
pub(crate) fn vma_hold<T: Send + Sync>(
    vma: &VmaNew,
    ops: &'static SyncStatic<bindings::vm_operations_struct>,
    data: Arc<T>,
) {
    // SAFETY: The VMA is under construction; no other code reads these fields
    // yet. `ops.open`/`ops.close` keep the reference count in step with VMA
    // copies and splits.
    unsafe {
        (*vma.as_ptr()).vm_ops = &ops.0;
        (*vma.as_ptr()).vm_private_data = Arc::into_raw(data).cast_mut().cast();
    }
}

/// The `Arc<T>` attached by [`vma_hold`] to the VMA containing `addr` in the
/// current process, if that VMA uses `ops`. Takes the caller's `mmap_lock`.
pub(crate) fn current_vma_data<T: Send + Sync>(
    ops: &'static SyncStatic<bindings::vm_operations_struct>,
    addr: u64,
    len: u64,
) -> Option<(Arc<T>, u64)> {
    let task = kernel::current!();
    let mm = task.mm()?;
    let guard = mm.mmap_read_lock();
    let vma = guard.vma_lookup(usize::try_from(addr).ok()?)?;
    let end = addr.checked_add(len)?;
    if end > vma.end() as u64 {
        return None;
    }
    let raw = vma.as_ptr();
    // SAFETY: We hold the `mmap_lock`, so the VMA and its fields are stable.
    let (vm_ops, private, pgoff) = unsafe {
        (
            (*raw).vm_ops,
            (*raw).vm_private_data,
            (*raw).vm_pgoff as u64,
        )
    };
    if !ptr::eq(vm_ops, &ops.0) || private.is_null() {
        return None;
    }
    // SAFETY: VMAs with `ops` hold an `Arc<T>` reference in `vm_private_data`
    // for as long as they exist, and the VMA cannot go away under the lock.
    let data: Arc<T> = unsafe { ArcBorrow::from_raw(private.cast::<T>()) }.into();
    let file_offset = pgoff * PAGE as u64 + (addr - vma.start() as u64);
    Some((data, file_offset))
}

/// `vm_operations_struct` callbacks that track an `Arc<T>` per VMA.
pub(crate) const fn vm_ops_for<T: Send + Sync>() -> bindings::vm_operations_struct {
    unsafe extern "C" fn open<T: Send + Sync>(vma: *mut bindings::vm_area_struct) {
        // SAFETY: `vma` is a copy of a VMA set up by `vma_hold`, whose
        // `vm_private_data` is a live `Arc<T>` pointer. Take one more reference
        // for the copy.
        unsafe {
            let data = (*vma).vm_private_data.cast::<T>();
            let arc: Arc<T> = ArcBorrow::from_raw(data).into();
            core::mem::forget(arc);
        }
    }
    unsafe extern "C" fn close<T: Send + Sync>(vma: *mut bindings::vm_area_struct) {
        // SAFETY: Each VMA owns one reference (from `vma_hold` or `open`).
        unsafe { drop(Arc::<T>::from_raw((*vma).vm_private_data.cast::<T>())) };
    }
    let mut ops: bindings::vm_operations_struct = pin_init::zeroed();
    ops.open = Some(open::<T>);
    ops.close = Some(close::<T>);
    ops
}

/// Remove user mappings of `[start, start + len)` (or everything when `len` is
/// zero) in `mapping`, including private copies. Accesses then fault (SIGBUS).
///
/// # Safety
///
/// `mapping` must be the `f_mapping` of a file that is still open.
pub(crate) unsafe fn zap_mapping(mapping: usize, start: u64, len: u64) {
    // SAFETY: Per the contract the address space is alive.
    unsafe {
        bindings::unmap_mapping_range(
            mapping as *mut bindings::address_space,
            start as bindings::loff_t,
            len as bindings::loff_t,
            1,
        )
    };
}

/// The address space of an open file, for [`zap_mapping`].
pub(crate) fn file_mapping(file: &kernel::fs::File) -> usize {
    // SAFETY: `f_mapping` of an open file is valid and fixed.
    unsafe { (*file.as_ptr()).f_mapping as usize }
}

/// Whether the caller has `CAP_SYS_ADMIN` in the initial user namespace.
pub(crate) fn capable_sys_admin() -> bool {
    // SAFETY: `capable()` only inspects the current task's credentials.
    unsafe { bindings::capable(bindings::CAP_SYS_ADMIN as c_int) }
}

// -------------------------------------------------------------------------------
// Small helpers
// -------------------------------------------------------------------------------

/// Write one entry of a device-visible table in coherent memory. Out-of-range
/// indices are ignored.
pub(crate) fn coherent_write_u64(table: &dma::Coherent<[u64]>, index: usize, value: u64) {
    if index >= table.len() {
        return;
    }
    // SAFETY: `index` is in bounds. The device may read the table concurrently,
    // so the store is volatile; there are no Rust references to the memory.
    unsafe {
        table
            .as_mut_ptr()
            .cast::<u64>()
            .add(index)
            .write_volatile(value)
    };
}

/// Order CPU writes to DMA memory before later writes that let the device read
/// it.
pub(crate) fn dma_wmb() {
    // SAFETY: A memory barrier has no preconditions.
    unsafe { bindings::dma_wmb() };
}

/// Run a pending delayed work item now and wait for it to finish.
pub(crate) fn flush_delayed_work<T: ?Sized, const ID: u64>(
    work: &kernel::workqueue::DelayedWork<T, ID>,
) {
    // `DelayedWork` is a transparent wrapper around `struct delayed_work`.
    let raw = core::ptr::from_ref(work)
        .cast_mut()
        .cast::<bindings::delayed_work>();
    // SAFETY: `work` was initialised by `DelayedWork::new()` and is pinned.
    unsafe { bindings::flush_delayed_work(raw) };
}

/// A PCI ID-table entry for any vendor.
///
/// `pci::DeviceId::from_id()` takes a `pci::Vendor`, which can only name vendors
/// listed in `pci_ids.h`; Global Unichip (0x1ac1) is not one of them.
pub(crate) const fn pci_device_id(vendor: u32, device: u32) -> pci::DeviceId {
    let mut raw: bindings::pci_device_id = pin_init::zeroed();
    raw.vendor = vendor;
    raw.device = device;
    raw.subvendor = !0;
    raw.subdevice = !0;
    // SAFETY: `pci::DeviceId` is `#[repr(transparent)]` over
    // `bindings::pci_device_id`, so both have the same layout and validity.
    unsafe { core::mem::transmute::<bindings::pci_device_id, pci::DeviceId>(raw) }
}

/// Cancel a pending run of the delayed work item embedded in `this` (enqueued
/// as an `Arc<T>`), releasing the reference the queue held. Returns whether a
/// run was pending. Does not wait for a run in progress; callable under locks.
pub(crate) fn cancel_arc_delayed_work<T: Send + Sync, const ID: u64>(
    this: &Arc<T>,
    work: &kernel::workqueue::DelayedWork<T, ID>,
) -> bool {
    let raw = core::ptr::from_ref(work)
        .cast_mut()
        .cast::<bindings::delayed_work>();
    // SAFETY: `work` was initialised by `DelayedWork::new()` and is pinned.
    let cancelled = unsafe { bindings::cancel_delayed_work(raw) };
    if cancelled {
        // SAFETY: Enqueueing turned one `Arc<T>` into the queue entry, which the
        // work function would have converted back. The entry was cancelled before
        // running, so that reference is ours to release; it is the same
        // allocation as `this`.
        drop(unsafe { Arc::from_raw(Arc::as_ptr(this)) });
    }
    cancelled
}

// -------------------------------------------------------------------------------
// dma-buf import
// -------------------------------------------------------------------------------

extern "C" {
    // Exported (DMA_BUF namespace) but not covered by the kernel crate's bindings.
    // Opaque pointers are passed as `c_void`: their bindings are not FFI-safe
    // by value (lockdep maps), and only the pointer crosses the boundary.
    fn dma_buf_unmap_attachment_unlocked(
        attach: *mut c_void,
        sgt: *mut c_void,
        dir: bindings::dma_data_direction,
    );
}

/// A dma-buf attached to the device and mapped for DMA.
pub(crate) struct DmaBufMapping {
    dmabuf: NonNull<bindings::dma_buf>,
    attach: *mut bindings::dma_buf_attachment,
    sgt: *mut bindings::sg_table,
    dir: dma::DataDirection,
}

// SAFETY: dma-buf references and attachments are not tied to a thread.
unsafe impl Send for DmaBufMapping {}
// SAFETY: Shared access only reads the mapped table.
unsafe impl Sync for DmaBufMapping {}

fn dma_buf_from_fd(fd: i32) -> Result<NonNull<bindings::dma_buf>> {
    // SAFETY: `dma_buf_get()` validates `fd` and returns a counted reference.
    let buf = from_err_ptr(unsafe { bindings::dma_buf_get(fd) })?;
    NonNull::new(buf).ok_or(EBADF)
}

impl DmaBufMapping {
    /// Import the dma-buf behind `fd` for DMA by `dev`; it must hold at least
    /// `min_bytes`.
    pub(crate) fn new(
        dev: &device::Device<Bound>,
        fd: i32,
        dir: dma::DataDirection,
        min_bytes: u64,
    ) -> Result<Self> {
        let dmabuf = dma_buf_from_fd(fd)?;
        let mut this = Self {
            dmabuf,
            attach: ptr::null_mut(),
            sgt: ptr::null_mut(),
            dir,
        };
        // SAFETY: We hold a reference to `dmabuf`.
        if (unsafe { (*dmabuf.as_ptr()).size } as u64) < min_bytes {
            return Err(EINVAL);
        }
        // SAFETY: `dmabuf` is referenced and `dev` is a bound device.
        this.attach =
            from_err_ptr(unsafe { bindings::dma_buf_attach(dmabuf.as_ptr(), dev_raw(dev)) })?;
        // SAFETY: `attach` is a live attachment of `dmabuf`.
        this.sgt = from_err_ptr(unsafe {
            bindings::dma_buf_map_attachment_unlocked(this.attach, dir.into())
        })?;
        Ok(this)
    }

    /// Whether `fd` refers to this dma-buf.
    pub(crate) fn is_fd(&self, fd: i32) -> bool {
        match dma_buf_from_fd(fd) {
            Ok(other) => {
                let same = other == self.dmabuf;
                // SAFETY: Drop the reference taken just above.
                unsafe { bindings::dma_buf_put(other.as_ptr()) };
                same
            }
            Err(_) => false,
        }
    }

    /// DMA addresses of the buffer's 4 KiB pages, in order.
    pub(crate) fn dma_pages(&self) -> impl Iterator<Item = u64> + '_ {
        // SAFETY: `sgt` is the mapped table, valid until `self` drops.
        let table = unsafe { SGTable::from_raw(self.sgt) };
        table.iter().flat_map(|sg| {
            let base = sg.dma_address();
            let len = sg.dma_len();
            (0..len / PAGE as u64).map(move |i| base + i * PAGE as u64)
        })
    }
}

impl Drop for DmaBufMapping {
    fn drop(&mut self) {
        // SAFETY: Each step undoes the matching successful step of `new()`; the
        // driver drops mappings only while the device is bound.
        unsafe {
            if !self.sgt.is_null() {
                dma_buf_unmap_attachment_unlocked(
                    self.attach.cast(),
                    self.sgt.cast(),
                    self.dir.into(),
                );
            }
            if !self.attach.is_null() {
                bindings::dma_buf_detach(self.dmabuf.as_ptr(), self.attach);
            }
            bindings::dma_buf_put(self.dmabuf.as_ptr());
        }
    }
}

// -------------------------------------------------------------------------------
// hwmon
// -------------------------------------------------------------------------------

extern "C" {
    // Exported by the hwmon core; not covered by the kernel crate's bindings.
    // `struct device` is passed as `c_void` for the same reason as above.
    fn hwmon_device_register_with_groups(
        dev: *mut c_void,
        name: *const c_char,
        drvdata: *mut c_void,
        groups: *const *const bindings::attribute_group,
    ) -> *mut c_void;
    fn hwmon_device_unregister(dev: *mut c_void);
}

/// A hwmon device whose attributes reach an `Arc<T>` through the driver data,
/// like [`ClassDevice`].
pub(crate) struct HwmonDevice<T: Send + Sync> {
    dev: NonNull<bindings::device>,
    drvdata: *const T,
}

// SAFETY: As for `ClassDevice`.
unsafe impl<T: Send + Sync> Send for HwmonDevice<T> {}
// SAFETY: As for `ClassDevice`.
unsafe impl<T: Send + Sync> Sync for HwmonDevice<T> {}

impl<T: Send + Sync> HwmonDevice<T> {
    /// Register hwmon device `name` (static, no '-' or spaces) below `parent`.
    pub(crate) fn register(
        parent: &device::Device<Bound>,
        name: &'static CStr,
        groups: &'static SyncStatic<[*const bindings::attribute_group; 2]>,
        drvdata: Arc<T>,
    ) -> Result<Self> {
        let data = Arc::into_raw(drvdata);
        // SAFETY: `parent` is bound, `name` and `groups` are static.
        let dev = unsafe {
            hwmon_device_register_with_groups(
                dev_raw(parent).cast(),
                name.as_ptr().cast(),
                data.cast_mut().cast(),
                groups.0.as_ptr(),
            )
        }
        .cast::<bindings::device>();
        match from_err_ptr(dev).ok().and_then(NonNull::new) {
            Some(dev) => Ok(Self { dev, drvdata: data }),
            None => {
                // SAFETY: Reclaim the reference handed out above.
                drop(unsafe { Arc::from_raw(data) });
                Err(from_err_ptr(dev).err().unwrap_or(ENOMEM))
            }
        }
    }
}

impl<T: Send + Sync> Drop for HwmonDevice<T> {
    fn drop(&mut self) {
        // SAFETY: Registered in `register()`; this waits for sysfs callbacks.
        unsafe { hwmon_device_unregister(self.dev.as_ptr().cast()) };
        // SAFETY: No callback can observe `drvdata` any more.
        drop(unsafe { Arc::from_raw(self.drvdata) });
    }
}

// -------------------------------------------------------------------------------
// Power management
// -------------------------------------------------------------------------------

/// Install `ops` as the PM callbacks of the driver `pdev` is bound to.
///
/// The Rust PCI abstraction does not expose `dev_pm_ops` yet. The driver
/// structure it registers is ordinary writable memory owned by this module;
/// probe runs before any system sleep transition can reach the device (the PM
/// core waits for probing), and every probe stores the same pointer.
pub(crate) fn install_pm_ops(
    pdev: &pci::Device<Bound>,
    ops: &'static SyncStatic<bindings::dev_pm_ops>,
) {
    let dev = dev_raw(pdev.as_ref());
    // SAFETY: A bound device has a valid `driver` pointer to our `pci_driver`'s
    // embedded `device_driver`.
    unsafe {
        let drv = (*dev).driver;
        if !drv.is_null() {
            (*drv).pm = &ops.0;
        }
    }
}
