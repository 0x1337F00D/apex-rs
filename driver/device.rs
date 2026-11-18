// SPDX-License-Identifier: GPL-2.0-only

//! Per-device state shared by the PCI driver, open files, sysfs and IRQs.
//!
//! # Locking and lifetime
//!
//! All mutable state lives in [`State`] behind one mutex. `State::hw` holds the
//! CSR BAR and is `Some` exactly while the PCI device is bound: the unbind path
//! takes the mutex, tears everything down and sets it to `None` before it
//! returns. Holding the mutex with `hw` present therefore proves the device is
//! bound, which is what [`Shared::bound`] relies on. Open files outlive unbind;
//! their operations then fail with `ENODEV` instead of touching freed state.
//!
//! Lock order: `mmap_lock` → `State` → `i_mmap_rwsem`. Nothing that may take a
//! process's `mmap_lock` (page pinning, VMA lookups) runs under `State`.
//!
//! Interrupt handlers take no locks: eventfds are swapped under RCU (see
//! [`sys::EventSlot`]) and counters are atomic.

use crate::{
    abi::{self, Direction},
    hw::{self, Csr, Subtables, TableBackend, TempWarn},
    pagetable::{self, PageTable, Range},
    regs, sys,
    thermal::{self, Trips},
    COHERENT_VM_OPS,
};
use kernel::{
    device::{self, Bound},
    devres::DevresLt,
    dma::{self, Coherent},
    fs::File,
    mm::virt::{flags as vmflags, VmaNew},
    new_mutex, pci,
    prelude::*,
    rbtree::{RBTree, RBTreeNodeReservation},
    sync::aref::ARef,
    sync::{
        atomic::{ordering, Atomic},
        Arc, Mutex,
    },
    task::Pid,
    time::msecs_to_jiffies,
    workqueue::{self, impl_has_delayed_work, new_delayed_work, DelayedWork, WorkItem},
};

/// The CSR BAR as stored for the lifetime of the binding.
pub(crate) type CsrBar = DevresLt<pci::Bar<'static, { regs::CSR_BAR_BYTES }>>;

/// Behaviour selected by module parameters at probe time.
#[derive(Clone, Copy)]
pub(crate) struct Params {
    pub(crate) allow_power_save: bool,
    pub(crate) allow_sw_clock_gating: bool,
    pub(crate) allow_hw_clock_gating: bool,
    pub(crate) temp_poll_interval_ms: u32,
    /// Testing only: treat every resume as if the chip lost its state.
    pub(crate) test_resume_state_loss: bool,
}

/// gasket's `enum gasket_status`, reduced to what the driver can observe.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Alive,
    /// A reset failed or the device is gone.
    Dead,
}

/// Identity of one allocation of the coherent host-queue buffer
/// (`GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR`).
///
/// The memory itself is owned by [`State`] and freed while the device is bound.
/// User VMAs and device mappings of it hold this token only, which lets
/// `MAP_BUFFER` recognise the buffer and lets a free find and revoke every
/// mapping first. gasket freed the memory while it was still mapped into user
/// space and into the device, and leaked it when allocated twice.
pub(crate) struct CoherentBuf {
    len: usize,
}

/// What backs a device mapping.
enum Backing {
    /// Pinned user memory. Fields drop in order: DMA unmap, then unpin.
    User {
        dma: sys::DmaSgt,
        _pages: sys::PinnedPages,
    },
    /// Part of the coherent buffer, `offset` bytes in.
    Coherent { buf: Arc<CoherentBuf>, offset: u64 },
    /// An imported dma-buf (GASKET_IOCTL_MAP_DMABUF).
    DmaBuf(sys::DmaBufMapping),
}

/// A buffer mapped into the device address space.
struct Mapping {
    range: Range,
    /// Held for its `Drop`: unmapping and unpinning happen when the record goes.
    backing: Backing,
}

/// Mutable device state, see the module documentation.
pub(crate) struct State {
    hw: Option<CsrBar>,
    status: Status,
    owner: Option<Pid>,
    write_open_count: u32,
    reset_count: u64,
    pt: KVBox<PageTable>,
    subtables: Subtables,
    /// Keyed by page-aligned device address.
    mappings: RBTree<u64, Mapping>,
    /// The current coherent buffer: its identity token and its memory.
    coherent: Option<(Arc<CoherentBuf>, Coherent<[u8]>)>,
    /// `f_mapping` of every open file, to revoke user mappings on unbind.
    files: KVec<usize>,
    /// GCB clock divider requested through PERFORMANCE_EXPECTATION.
    user_divider: u32,
    /// Divider required by the temperature (0 = not throttled).
    thermal_level: u32,
    trips: Trips,
    warn1: TempWarn,
    warn2: TempWarn,
    temp_poll_interval_ms: u32,
    poll_stopped: bool,
}

// SAFETY: `PageTable` consists only of integers and arrays of integers.
unsafe impl pin_init::Zeroable for PageTable {}

/// State shared between all users of one Apex device.
#[pin_data]
pub(crate) struct Shared {
    pdev: ARef<pci::Device>,
    pub(crate) index: u32,
    params: Params,
    #[pin]
    state: Mutex<State>,
    events: [sys::EventSlot; regs::NUM_INTERRUPTS],
    irq_counts: [Atomic<u64>; regs::NUM_INTERRUPTS],
    #[pin]
    thermal_work: DelayedWork<Shared>,
}

impl_has_delayed_work! {
    impl HasDelayedWork<Self> for Shared { self.thermal_work }
}

impl WorkItem for Shared {
    type Pointer = Arc<Shared>;

    fn run(this: Arc<Shared>) {
        this.thermal_poll();
    }
}

fn pt_err(e: pagetable::Error) -> Error {
    match e {
        pagetable::Error::Invalid => EINVAL,
        pagetable::Error::Busy => EBUSY,
        pagetable::Error::NoMemory => ENOMEM,
    }
}

fn hw_warn_from_millic(millic: i32, enabled: bool) -> TempWarn {
    TempWarn {
        adc: thermal::millic_to_adc(millic).unwrap_or(0),
        enabled,
    }
}

impl Shared {
    pub(crate) fn new(
        pdev: &pci::Device<Bound>,
        index: u32,
        params: Params,
        bar: CsrBar,
    ) -> Result<impl PinInit<Self, Error>> {
        let mut pt: KVBox<PageTable> = KVBox::init(pin_init::zeroed::<PageTable>(), GFP_KERNEL)?;
        pt.init();
        let state = State {
            hw: Some(bar),
            status: Status::Alive,
            owner: None,
            write_open_count: 0,
            reset_count: 0,
            pt,
            subtables: Subtables::new()?,
            mappings: RBTree::new(),
            coherent: None,
            files: KVec::new(),
            user_divider: 0,
            thermal_level: 0,
            trips: Trips::defaults(),
            // apex_driver.c defaults: warn1 (INTR) off, warn2 (SD_ALARM) on, both 100 °C.
            warn1: hw_warn_from_millic(100_000, false),
            warn2: hw_warn_from_millic(100_000, true),
            temp_poll_interval_ms: params.temp_poll_interval_ms,
            poll_stopped: false,
        };
        let pdev: ARef<pci::Device> = (&**pdev).into();
        Ok(try_pin_init!(Self {
            pdev,
            index,
            params,
            state <- new_mutex!(state),
            events: core::array::from_fn(|_| sys::EventSlot::new()),
            irq_counts: core::array::from_fn(|_| Atomic::new(0)),
            thermal_work <- new_delayed_work!("apex::thermal"),
        }? Error))
    }

    /// The bound device. Takes the BAR slot of a locked [`State`] as proof.
    fn bound<'a>(&'a self, hw: &'a Option<CsrBar>) -> Result<(&'a device::Device<Bound>, Csr<'a>)> {
        let bar = hw.as_ref().ok_or(ENODEV)?;
        let dev: &device::Device = self.pdev.as_ref();
        // SAFETY: `hw` is `Some` and borrowed from the locked `State`. The unbind
        // path clears it under the same lock before unbind completes, so the
        // device stays bound for as long as this borrow lives.
        let dev = unsafe { dev.as_bound() };
        let bar = bar.access(dev)?;
        Ok((dev, Csr::new(bar)))
    }

    fn pci_bound<'a>(&'a self, dev: &'a device::Device<Bound>) -> Result<&'a pci::Device<Bound>> {
        dev.try_into()
    }

    // ---------------------------------------------------------------------------
    // Probe-time setup
    // ---------------------------------------------------------------------------

    /// Bring the chip up after the IRQ vectors are in place, then park it in
    /// low-power reset until the first open.
    pub(crate) fn probe_init(&self) -> Result {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        self.chip_reset(st)?;
        {
            let (dev, csr) = self.bound(&st.hw)?;
            // gasket never read this register (it has a TODO to). Before the first
            // reset it reads all-ones on real hardware, so only a plausible value
            // that contradicts the fixed table size is treated as an error.
            match hw::page_table_size(csr) {
                n if n == pagetable::TOTAL as u64 || n == 0 || n == u64::MAX => {}
                n if n < pagetable::TOTAL as u64 => {
                    dev_err!(
                        dev,
                        "hardware reports only {} page table entries, need {}\n",
                        n,
                        pagetable::TOTAL
                    );
                    return Err(ENODEV);
                }
                n => dev_info!(
                    dev,
                    "hardware reports {} page table entries, using {}\n",
                    n,
                    pagetable::TOTAL
                ),
            }
        }
        if self.params.allow_power_save {
            let (dev, csr) = self.bound(&st.hw)?;
            hw::enter_reset(dev, csr)?;
        }
        Ok(())
    }

    /// Start the periodic temperature check.
    pub(crate) fn start_thermal_poll(self: &Arc<Self>) {
        let st = self.state.lock();
        self.schedule_poll(&st);
    }

    fn schedule_poll(self: &Arc<Self>, st: &State) {
        if st.poll_stopped || st.temp_poll_interval_ms == 0 {
            return;
        }
        let delay = msecs_to_jiffies(st.temp_poll_interval_ms);
        // Already queued is fine.
        let _ = workqueue::system_freezable_power_efficient().enqueue_delayed(self.clone(), delay);
    }

    // ---------------------------------------------------------------------------
    // Reset, open and close
    // ---------------------------------------------------------------------------

    /// gasket_reset_nolock(): reset the chip and rebuild everything a reset
    /// clears. Every device mapping is released.
    fn chip_reset(&self, st: &mut State) -> Result {
        let (dev, csr) = self.bound(&st.hw)?;
        let reset = hw::reset(dev, csr, self.params.allow_hw_clock_gating)
            .and_then(|()| hw::wait_tables_ready(dev, csr));

        // Translations and the buffers behind them go even if the reset failed:
        // gasket did the same, and keeping pages pinned forever is worse.
        let mut be = TableBackend {
            dev,
            csr,
            subtables: &mut st.subtables,
        };
        st.pt.reset(&mut be);
        drop(core::mem::replace(&mut st.mappings, RBTree::new()));

        let pdev = self.pci_bound(dev)?;
        sys::rearm_msix(pdev);
        hw::program_interrupt_routing(csr)?;
        hw::enable_thermal_sensing(csr);
        hw::program_temp_warnings(csr, st.warn1, st.warn2);
        hw::set_divider(
            csr,
            thermal::effective_divider(st.user_divider, st.thermal_level),
        );

        st.reset_count += 1;
        st.status = if reset.is_ok() {
            Status::Alive
        } else {
            Status::Dead
        };
        reset
    }

    /// Called for every open of the device node.
    pub(crate) fn open(&self, file: &File, writer: bool) -> Result {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        self.bound(&st.hw)?;
        if writer {
            let tgid = kernel::current!().tgid();
            match st.owner {
                Some(owner) if owner != tgid && !sys::capable_sys_admin() => return Err(EPERM),
                Some(_) => {}
                None => {
                    // First writer: fresh chip state, as gasket's device_open_cb.
                    self.chip_reset(st)?;
                    st.owner = Some(tgid);
                }
            }
        }
        st.files.push(sys::file_mapping(file), GFP_KERNEL)?;
        if writer {
            st.write_open_count += 1;
        }
        Ok(())
    }

    /// Called when an open file is finally released (after its last VMA is gone).
    pub(crate) fn release(&self, mapping: usize, writer: bool) {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        if let Some(i) = st.files.iter().position(|&m| m == mapping) {
            let _ = st.files.remove(i);
        }
        if !writer {
            return;
        }
        st.write_open_count = st.write_open_count.saturating_sub(1);
        if st.write_open_count > 0 {
            return;
        }
        st.owner = None;
        self.final_close(st);
    }

    /// Free the coherent buffer, after removing every device mapping into it and
    /// every user mapping of it (later accesses fault with SIGBUS). Runs while
    /// the device is bound, so the DMA memory never outlives the binding.
    fn revoke_coherent(&self, st: &mut State) {
        let Some((token, mem)) = st.coherent.take() else {
            return;
        };
        let mut keys = KVec::new();
        for (&key, m) in st.mappings.iter() {
            if matches!(&m.backing, Backing::Coherent { buf, .. } if Arc::ptr_eq(buf, &token)) {
                // On allocation failure the mapping is still removed by the next
                // reset; the memory is freed regardless below, so do not skip it.
                let _ = keys.push(key, GFP_KERNEL);
            }
        }
        if let Ok((dev, csr)) = self.bound(&st.hw) {
            let mut be = TableBackend {
                dev,
                csr,
                subtables: &mut st.subtables,
            };
            for key in keys.iter() {
                if let Some(m) = st.mappings.remove(key) {
                    st.pt.unmap(&mut be, m.range);
                }
            }
        }
        for &mapping in st.files.iter() {
            // SAFETY: Entries are removed in `release()` before a file is freed.
            unsafe { sys::zap_mapping(mapping, regs::COHERENT_MMAP_OFFSET, mem.size() as u64) };
        }
        drop(mem);
    }

    /// The owner closed its last writable file: reset, release every resource it
    /// held and enter low-power reset.
    fn final_close(&self, st: &mut State) {
        let Ok((dev, csr)) = self.bound(&st.hw) else {
            return;
        };
        let (hib, scalar) = hw::error_status(csr);
        dev_dbg!(
            dev,
            "release: hib_error {:#x} scalar_error {:#x}\n",
            hib,
            scalar
        );
        let _ = self.chip_reset(st);
        // Read-only openers may still map the buffer.
        self.revoke_coherent(st);
        // gasket kept the last owner's eventfds referenced until somebody set new
        // ones; drop them with the rest of its state.
        for slot in &self.events {
            slot.replace(None);
        }
        if self.params.allow_power_save {
            if let Ok((dev, csr)) = self.bound(&st.hw) {
                let _ = hw::enter_reset(dev, csr);
            }
        }
    }

    /// Unbind: stop the device, release all device resources while it is still
    /// bound and revoke user access. Open files keep working only as far as
    /// returning `ENODEV`.
    pub(crate) fn shutdown(&self) {
        {
            let mut guard = self.state.lock();
            let st = &mut *guard;
            st.poll_stopped = true;
            if let Ok((dev, csr)) = self.bound(&st.hw) {
                if !hw::gcb_in_reset(csr) {
                    let _ = hw::enter_reset(dev, csr);
                }
                let mut be = TableBackend {
                    dev,
                    csr,
                    subtables: &mut st.subtables,
                };
                st.pt.reset(&mut be);
                drop(core::mem::replace(&mut st.mappings, RBTree::new()));
            }
            // BAR and coherent pages must not stay reachable from user space
            // once the device is gone; later accesses fault with SIGBUS.
            for &mapping in st.files.iter() {
                // SAFETY: Entries are removed in `release()` before a file is freed.
                unsafe { sys::zap_mapping(mapping, 0, 0) };
            }
            self.revoke_coherent(st);
            st.subtables.clear();
            for slot in &self.events {
                slot.replace(None);
            }
            st.status = Status::Dead;
            st.owner = None;
            st.hw = None;
        }
        // The poll re-arms itself only under the lock and sees `poll_stopped`.
        sys::flush_delayed_work(&self.thermal_work);
    }

    // ---------------------------------------------------------------------------
    // ioctls
    // ---------------------------------------------------------------------------

    fn alive<'a>(
        &'a self,
        hw: &'a Option<CsrBar>,
        status: Status,
    ) -> Result<(&'a device::Device<Bound>, Csr<'a>)> {
        let r = self.bound(hw)?;
        if status != Status::Alive {
            return Err(EIO);
        }
        Ok(r)
    }

    /// GASKET_IOCTL_RESET.
    pub(crate) fn reset_ioctl(&self) -> Result {
        let mut guard = self.state.lock();
        self.chip_reset(&mut guard)
    }

    /// GASKET_IOCTL_SET_EVENTFD / CLEAR_EVENTFD.
    pub(crate) fn set_eventfd(&self, interrupt: u64, fd: Option<u64>) -> Result {
        let guard = self.state.lock();
        self.alive(&guard.hw, guard.status)?;
        let slot = usize::try_from(interrupt)
            .ok()
            .and_then(|i| self.events.get(i))
            .ok_or(EINVAL)?;
        let ctx = fd.map(sys::EventFd::from_fd).transpose()?;
        slot.replace(ctx);
        Ok(())
    }

    /// GASKET_IOCTL_CLEAR_INTERRUPT_COUNTS.
    pub(crate) fn clear_interrupt_counts(&self) {
        for c in &self.irq_counts {
            c.store(0, ordering::Relaxed);
        }
    }

    /// Hard-IRQ handler body for vector `vector`.
    pub(crate) fn interrupt(&self, vector: usize) {
        if let Some(c) = self.irq_counts.get(vector) {
            c.add(1, ordering::Relaxed);
        }
        if let Some(slot) = self.events.get(vector) {
            slot.signal();
        }
    }

    pub(crate) fn interrupt_count(&self, vector: usize) -> u64 {
        self.irq_counts[vector].load(ordering::Relaxed)
    }

    /// (total entries, simple entries, mapped pages).
    pub(crate) fn page_table_info(&self) -> (u64, u64, u64) {
        let st = self.state.lock();
        (
            st.pt.total_entries() as u64,
            st.pt.num_simple() as u64,
            st.pt.active_pages() as u64,
        )
    }

    /// GASKET_IOCTL_PARTITION_PAGE_TABLE.
    pub(crate) fn partition(&self, num_simple: u64) -> Result {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let (dev, csr) = self.alive(&st.hw, st.status)?;
        let mut be = TableBackend {
            dev,
            csr,
            subtables: &mut st.subtables,
        };
        st.pt.partition(&mut be, num_simple).map_err(pt_err)
    }

    /// GASKET_IOCTL_MAP_BUFFER(_FLAGS).
    pub(crate) fn map_buffer(
        &self,
        host: u64,
        dev_addr: u64,
        bytes: u64,
        dir: Direction,
    ) -> Result {
        let page = sys::PAGE as u64;
        if !host.is_multiple_of(page) || bytes == 0 || !bytes.is_multiple_of(page) {
            return Err(EINVAL);
        }
        let count = usize::try_from(bytes / page).map_err(|_| EINVAL)?;
        if count > pagetable::TOTAL * pagetable::SUBTABLE_ENTRIES {
            return Err(EINVAL);
        }

        // Resolve the host memory before taking the state lock (lock order).
        enum Source {
            User(sys::PinnedPages),
            Coherent(Arc<CoherentBuf>, u64),
        }
        let source = match sys::current_vma_data::<CoherentBuf>(&COHERENT_VM_OPS, host, bytes) {
            Some((buf, file_offset)) => {
                let offset = file_offset
                    .checked_sub(regs::COHERENT_MMAP_OFFSET)
                    .ok_or(EINVAL)?;
                if offset.checked_add(bytes).ok_or(EINVAL)? > buf.len as u64 {
                    return Err(EINVAL);
                }
                Source::Coherent(buf, offset)
            }
            None => Source::User(sys::PinnedPages::pin(host, count, dir.device_writes())?),
        };
        let reservation = RBTreeNodeReservation::new(GFP_KERNEL)?;

        let mut guard = self.state.lock();
        let st = &mut *guard;
        let (dev, csr) = self.alive(&st.hw, st.status)?;
        let range = st.pt.validate(dev_addr, bytes).map_err(pt_err)?;
        let key = dev_addr & !(page - 1);
        let backing = match source {
            Source::User(mut pages) => {
                let dma = sys::DmaSgt::new(dev, &mut pages, dma_dir(dir))?;
                Backing::User { dma, _pages: pages }
            }
            Source::Coherent(buf, offset) => Backing::Coherent { buf, offset },
        };
        {
            let mut be = TableBackend {
                dev,
                csr,
                subtables: &mut st.subtables,
            };
            let result = match &backing {
                Backing::User { dma, .. } => st.pt.map(&mut be, range, &mut dma.dma_pages()),
                Backing::Coherent { buf, offset } => {
                    // Only the current allocation may be mapped; a stale VMA of a
                    // revoked one is refused.
                    let base = match &st.coherent {
                        Some((current, mem)) if Arc::ptr_eq(current, buf) => {
                            mem.dma_address() + offset
                        }
                        _ => return Err(EINVAL),
                    };
                    st.pt.map(
                        &mut be,
                        range,
                        &mut (0..count as u64).map(|i| base + i * page),
                    )
                }
                Backing::DmaBuf(_) => unreachable!("map_buffer never builds dma-buf backings"),
            };
            result.map_err(pt_err)?;
        }
        // A free range cannot have a mapping record at its start address.
        let old = st
            .mappings
            .insert(reservation.into_node(key, Mapping { range, backing }));
        debug_assert!(old.is_none());
        Ok(())
    }

    /// GASKET_IOCTL_UNMAP_BUFFER. The range must be exactly one earlier mapping
    /// (gasket would unmap any range, including parts of other buffers).
    pub(crate) fn unmap_buffer(&self, dev_addr: u64, bytes: u64) -> Result {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let (dev, csr) = self.alive(&st.hw, st.status)?;
        let range = st.pt.validate(dev_addr, bytes).map_err(pt_err)?;
        let key = dev_addr & !(sys::PAGE as u64 - 1);
        match st.mappings.get(&key) {
            Some(m) if m.range == range => {}
            _ => return Err(EINVAL),
        }
        let mut be = TableBackend {
            dev,
            csr,
            subtables: &mut st.subtables,
        };
        st.pt.unmap(&mut be, range);
        // Dropping the record unmaps the DMA and unpins the pages.
        drop(st.mappings.remove(&key));
        Ok(())
    }

    /// GASKET_IOCTL_MAP_DMABUF with `map` set: map `pages` pages of the dma-buf
    /// behind `fd`.
    pub(crate) fn map_dmabuf(&self, fd: i32, dev_addr: u64, pages: u32, dir: Direction) -> Result {
        let page = sys::PAGE as u64;
        let bytes = u64::from(pages) * page;
        let reservation = RBTreeNodeReservation::new(GFP_KERNEL)?;
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let (dev, csr) = self.alive(&st.hw, st.status)?;
        let range = st.pt.validate(dev_addr, bytes).map_err(pt_err)?;
        if !st.pt.is_free(range) {
            return Err(EBUSY);
        }
        let buf = sys::DmaBufMapping::new(dev, fd, dma_dir(dir), bytes)?;
        {
            let mut be = TableBackend {
                dev,
                csr,
                subtables: &mut st.subtables,
            };
            st.pt
                .map(&mut be, range, &mut buf.dma_pages().take(pages as usize))
                .map_err(pt_err)?;
        }
        let key = dev_addr & !(page - 1);
        let old = st.mappings.insert(reservation.into_node(
            key,
            Mapping {
                range,
                backing: Backing::DmaBuf(buf),
            },
        ));
        debug_assert!(old.is_none());
        Ok(())
    }

    /// GASKET_IOCTL_MAP_DMABUF with `map` clear: undo a `map_dmabuf()` of the
    /// same dma-buf, address and size.
    pub(crate) fn unmap_dmabuf(&self, fd: i32, dev_addr: u64, pages: u32) -> Result {
        let page = sys::PAGE as u64;
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let (dev, csr) = self.alive(&st.hw, st.status)?;
        let range = st
            .pt
            .validate(dev_addr, u64::from(pages) * page)
            .map_err(pt_err)?;
        let key = dev_addr & !(page - 1);
        match st.mappings.get(&key) {
            Some(Mapping {
                range: r,
                backing: Backing::DmaBuf(buf),
            }) if *r == range && buf.is_fd(fd) => {}
            _ => return Err(EINVAL),
        }
        let mut be = TableBackend {
            dev,
            csr,
            subtables: &mut st.subtables,
        };
        st.pt.unmap(&mut be, range);
        drop(st.mappings.remove(&key));
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // System sleep
    // ---------------------------------------------------------------------------

    pub(crate) fn pdev(&self) -> &pci::Device {
        &self.pdev
    }

    /// System suspend. User space is frozen. A closed device already sits in
    /// low-power reset. An open one is left alone: libedgetpu only pauses DMA
    /// right before a reset, there is no known way to resume it, and the chip
    /// may well keep its state (s2idle, D3hot).
    pub(crate) fn suspend(&self) {
        let guard = self.state.lock();
        if let Ok((dev, _)) = self.bound(&guard.hw) {
            dev_dbg!(
                dev,
                "suspend ({})\n",
                if guard.owner.is_some() {
                    "open"
                } else {
                    "closed"
                }
            );
        }
    }

    /// System resume. The PCI core has restored config space and MSI-X.
    /// Check whether the chip kept its programmed state; if it did not, a
    /// closed device is simply re-initialised, while an open one is marked
    /// dead so its owner gets errors instead of a hung queue, and recovers
    /// on the next open.
    pub(crate) fn resume(&self) {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let Ok((dev, csr)) = self.bound(&st.hw) else {
            return;
        };
        let intact = hw::interrupt_routing_intact(csr) && !self.params.test_resume_state_loss;
        if st.owner.is_none() {
            if !intact {
                dev_info!(dev, "re-initialising after power loss\n");
            }
            let _ = self.chip_reset(st);
            if self.params.allow_power_save {
                if let Ok((dev, csr)) = self.bound(&st.hw) {
                    let _ = hw::enter_reset(dev, csr);
                }
            }
        } else if intact {
            // The routing survived, but the MSI-X table may still have been
            // re-initialised (it lives in the chip); re-arming is harmless.
            if let Ok(pdev) = self.pci_bound(dev) {
                sys::rearm_msix(pdev);
            }
            hw::enable_thermal_sensing(csr);
            hw::program_temp_warnings(csr, st.warn1, st.warn2);
            hw::set_divider(
                csr,
                thermal::effective_divider(st.user_divider, st.thermal_level),
            );
            dev_dbg!(dev, "resumed with state intact\n");
        } else {
            // The model and queues the owner set up are gone, and it would wait
            // forever for completions. Make it fail now: revoke its mappings (the
            // next register access raises SIGBUS), wake every thread blocked on
            // an interrupt eventfd so that access happens immediately, and fail
            // its ioctls. Its final close resets the chip for the next user.
            dev_warn!(
                dev,
                "lost its state across suspend; revoking the current user's access\n"
            );
            st.status = Status::Dead;
            for &mapping in st.files.iter() {
                // SAFETY: Entries are removed in `release()` before a file is freed.
                unsafe { sys::zap_mapping(mapping, 0, 0) };
            }
            for slot in &self.events {
                slot.signal();
            }
            let _ = hw::program_interrupt_routing(csr);
            hw::enable_thermal_sensing(csr);
            hw::program_temp_warnings(csr, st.warn1, st.warn2);
        }
    }

    /// GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR; returns the mmap offset.
    pub(crate) fn config_coherent(&self, enable: bool, size: u64, dma_address: u64) -> Result<u64> {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let (dev, _) = self.alive(&st.hw, st.status)?;
        if !enable {
            if dma_address != regs::COHERENT_MMAP_OFFSET {
                return Err(EADDRNOTAVAIL);
            }
            self.revoke_coherent(st);
            return Ok(dma_address);
        }
        if size == 0 {
            return Err(EINVAL);
        }
        if size > regs::COHERENT_MAX_BYTES {
            return Err(ENOMEM);
        }
        if st.coherent.is_some() {
            // gasket overwrote (and leaked) the previous buffer.
            return Err(EBUSY);
        }
        let len = (size as usize).next_multiple_of(sys::PAGE);
        let mem = Coherent::<u8>::zeroed_slice(dev, len, GFP_KERNEL)?;
        st.coherent = Some((Arc::new(CoherentBuf { len }, GFP_KERNEL)?, mem));
        Ok(regs::COHERENT_MMAP_OFFSET)
    }

    /// APEX_IOCTL_GATE_CLOCK.
    pub(crate) fn gate_clock(&self, enable: bool) -> Result {
        if !self.params.allow_sw_clock_gating {
            return Ok(());
        }
        let guard = self.state.lock();
        let (_, csr) = self.alive(&guard.hw, guard.status)?;
        hw::gate_clock(csr, enable);
        Ok(())
    }

    /// APEX_IOCTL_PERFORMANCE_EXPECTATION.
    pub(crate) fn set_performance(&self, performance: u32) -> Result {
        let divider = abi::performance_clock_divider(performance).ok_or(EINVAL)?;
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let (_, csr) = self.alive(&st.hw, st.status)?;
        st.user_divider = divider;
        hw::set_clocks(csr, thermal::effective_divider(divider, st.thermal_level));
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // mmap
    // ---------------------------------------------------------------------------

    pub(crate) fn mmap(&self, vma: &VmaNew, writer: bool) -> Result {
        // Refuse copy-on-write mappings of device memory. (A shared mapping of a
        // read-only file has VM_SHARED and VM_MAYWRITE cleared by the core and
        // is fine.)
        if vma.flags() & (vmflags::SHARED | vmflags::MAYWRITE) == vmflags::MAYWRITE {
            return Err(EINVAL);
        }
        if vma.executable() {
            return Err(EPERM);
        }
        vma.try_clear_mayexec()?;

        let guard = self.state.lock();
        let st = &*guard;
        let (dev, _) = self.bound(&st.hw)?;
        let admin = sys::capable_sys_admin();
        let is_owner = writer && st.owner == Some(kernel::current!().tgid());
        if !admin {
            if st.status != Status::Alive {
                return Err(EPERM);
            }
            if vma.writable() && !is_owner {
                return Err(EPERM);
            }
        }
        if !admin && !is_owner {
            // Without this, a read-only mapping could be upgraded with mprotect().
            vma.try_clear_maywrite()?;
        }

        let offset = sys::vma_pgoff(vma)
            .checked_mul(sys::PAGE as u64)
            .ok_or(EINVAL)?;
        let len = (vma.end() - vma.start()) as u64;
        let coherent_len = st.coherent.as_ref().map_or(0, |(_, mem)| mem.size() as u64);
        match regs::classify_mmap(offset, len, coherent_len).ok_or(EINVAL)? {
            regs::MmapTarget::Csr { .. } => {
                sys::mmap_bar(self.pci_bound(dev)?, regs::CSR_BAR, vma)?;
                vma.set_dontcopy();
            }
            regs::MmapTarget::Coherent { offset } => {
                let (token, mem) = st.coherent.as_ref().ok_or(EINVAL)?;
                sys::mmap_coherent(dev, vma, mem, offset)?;
                vma.set_dontcopy();
                vma.set_dontexpand();
                sys::vma_hold(vma, &COHERENT_VM_OPS, token.clone());
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Thermal management and sysfs accessors
    // ---------------------------------------------------------------------------

    fn thermal_poll(self: Arc<Self>) {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        if st.poll_stopped {
            return;
        }
        if let Ok((dev, csr)) = self.bound(&st.hw) {
            let adc = hw::temperature_adc(csr);
            let level = st.trips.next_level(st.thermal_level, adc);
            if level != st.thermal_level {
                let millic = thermal::adc_to_millic(adc);
                if level > st.thermal_level {
                    dev_warn!(dev, "throttling to level {} at {} mC\n", level, millic);
                } else {
                    dev_info!(dev, "throttle level {} at {} mC\n", level, millic);
                }
                st.thermal_level = level;
                hw::set_divider(csr, thermal::effective_divider(st.user_divider, level));
            }
        }
        self.schedule_poll(st);
    }

    pub(crate) fn with_csr<R>(&self, f: impl FnOnce(Csr<'_>) -> R) -> Result<R> {
        let guard = self.state.lock();
        let (_, csr) = self.bound(&guard.hw)?;
        Ok(f(csr))
    }

    pub(crate) fn temperature_millic(&self) -> Result<i32> {
        self.with_csr(|csr| thermal::adc_to_millic(hw::temperature_adc(csr)))
    }

    pub(crate) fn trip(&self, i: usize) -> i32 {
        self.state.lock().trips.millic(i)
    }

    pub(crate) fn set_trip(&self, i: usize, millic: i32) -> Result {
        if self.state.lock().trips.set(i, millic) {
            Ok(())
        } else {
            Err(EINVAL)
        }
    }

    /// Hardware alarm `n` (1 or 2): (milli-°C, enabled).
    pub(crate) fn hw_warn(&self, n: u32) -> (i32, bool) {
        let st = self.state.lock();
        let w = if n == 1 { st.warn1 } else { st.warn2 };
        (thermal::adc_to_millic(w.adc), w.enabled)
    }

    pub(crate) fn set_hw_warn(&self, n: u32, millic: Option<i32>, enabled: Option<bool>) -> Result {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        let mut w = if n == 1 { st.warn1 } else { st.warn2 };
        if let Some(m) = millic {
            w.adc = thermal::millic_to_adc(m).ok_or(EINVAL)?;
        }
        if let Some(e) = enabled {
            w.enabled = e;
        }
        let (_, csr) = self.bound(&st.hw)?;
        if n == 1 {
            st.warn1 = w;
        } else {
            st.warn2 = w;
        }
        hw::program_temp_warnings(csr, st.warn1, st.warn2);
        Ok(())
    }

    pub(crate) fn temp_poll_interval(&self) -> u32 {
        self.state.lock().temp_poll_interval_ms
    }

    pub(crate) fn set_temp_poll_interval(self: &Arc<Self>, ms: u32) {
        let mut st = self.state.lock();
        st.temp_poll_interval_ms = ms;
        // Re-arm with the new interval instead of waiting for the old timer. A
        // poll that is running right now re-arms itself under this lock.
        sys::cancel_arc_delayed_work(self, &self.thermal_work);
        self.schedule_poll(&st);
    }

    pub(crate) fn throttle(&self) -> (u32, u32, u32) {
        let st = self.state.lock();
        let effective = thermal::effective_divider(st.user_divider, st.thermal_level);
        let programmed = self
            .bound(&st.hw)
            .map(|(_, c)| hw::divider(c))
            .unwrap_or(effective);
        (st.thermal_level, effective, programmed)
    }

    pub(crate) fn unique_id(&self) -> Result<[u32; 4]> {
        self.with_csr(hw::unique_id)
    }

    /// (alive, owner tgid or 0, write opens, resets).
    pub(crate) fn ownership(&self) -> (bool, Pid, u32, u64) {
        let st = self.state.lock();
        (
            st.status == Status::Alive && st.hw.is_some(),
            st.owner.unwrap_or(0),
            st.write_open_count,
            st.reset_count,
        )
    }
}

fn dma_dir(dir: Direction) -> dma::DataDirection {
    match dir {
        Direction::Bidirectional => dma::DataDirection::Bidirectional,
        Direction::ToDevice => dma::DataDirection::ToDevice,
        Direction::FromDevice => dma::DataDirection::FromDevice,
    }
}
