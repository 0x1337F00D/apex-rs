// SPDX-License-Identifier: GPL-2.0-only
// Copyright (C) 2018 Google, Inc.: derived from the gasket/apex driver.

//! Rust driver for the Google Coral Edge TPU ("Apex") PCIe accelerator.
//!
//! A drop-in replacement for Google's out-of-tree `gasket` + `apex` modules: the
//! same `/dev/apex_N` ioctls and mmap layout, the same `/sys/class/apex`
//! attributes, so the unmodified libedgetpu runtime works on top of it.
//!
//! Module layout:
//! - [`regs`], [`abi`], [`pagetable`], [`thermal`], [`text`]: hardware-independent
//!   logic, also built and tested on the host (`hosttest/`).
//! - [`sys`]: safe wrappers for the C APIs the kernel crate lacks; the driver's
//!   raw FFI lives there.
//! - [`hw`]: register sequences. [`device`]: shared state and the operations.
//! - [`file`]: the misc device. [`sysfs`]: the `apex` class and attributes.

mod abi;
mod device;
mod file;
mod hw;
mod pagetable;
mod regs;
mod sys;
mod sysfs;
mod text;
mod thermal;

use core::pin::Pin;
use device::{CoherentBuf, Params, Shared};
use file::ApexFile;
use kernel::{
    bindings,
    device::{Bound, Core},
    dma::{self, DmaMask},
    driver,
    irq::{self, IrqReturn},
    miscdevice::{MiscDeviceOptions, MiscDeviceRegistration},
    pci,
    prelude::*,
    sync::{
        atomic::{ordering, Atomic},
        Arc,
    },
};

pub(crate) const DRIVER_VERSION: &str = "1.2-rs.1";

/// Most devices handled at once (the size of the name and index tables).
const MAX_DEVICES: usize = 16;

/// Device node and IRQ names. They must be `'static`.
static NAMES: [&CStr; MAX_DEVICES] = [
    c"apex_0", c"apex_1", c"apex_2", c"apex_3", c"apex_4", c"apex_5", c"apex_6", c"apex_7",
    c"apex_8", c"apex_9", c"apex_10", c"apex_11", c"apex_12", c"apex_13", c"apex_14", c"apex_15",
];

/// Bitmap of device indices in use.
static INDICES: Atomic<u32> = Atomic::new(0);

/// The `vm_operations_struct` of coherent-buffer mappings. It tracks one
/// `Arc<CoherentBuf>` reference per VMA.
pub(crate) static COHERENT_VM_OPS: sys::SyncStatic<bindings::vm_operations_struct> =
    sys::SyncStatic(sys::vm_ops_for::<CoherentBuf>());

/// Bound devices, for the PM callbacks: `REGISTRY_DEV[i]` holds the
/// `struct device` pointer of device `i` and `REGISTRY[i]` an `Arc<Shared>`
/// reference to its state. Slots are filled at the end of probe and emptied at
/// the start of unbind; both run under the device lock, as do PM callbacks.
static REGISTRY: [Atomic<*mut Shared>; MAX_DEVICES] =
    [const { Atomic::new(core::ptr::null_mut()) }; MAX_DEVICES];
static REGISTRY_DEV: [Atomic<usize>; MAX_DEVICES] = [const { Atomic::new(0) }; MAX_DEVICES];

fn registry_add(index: u32, shared: &Arc<Shared>) {
    let i = index as usize;
    REGISTRY[i].store(Arc::into_raw(shared.clone()).cast_mut(), ordering::Release);
    let dev: &kernel::device::Device = shared.pdev().as_ref();
    REGISTRY_DEV[i].store(core::ptr::from_ref(dev) as usize, ordering::Release);
}

fn registry_remove(index: u32) {
    let i = index as usize;
    REGISTRY_DEV[i].store(0, ordering::Release);
    let p = REGISTRY[i].xchg(core::ptr::null_mut(), ordering::Full);
    if !p.is_null() {
        // SAFETY: Stored by `registry_add()` from `Arc::into_raw()`.
        drop(unsafe { Arc::from_raw(p) });
    }
}

/// Run `f` on the state of the device whose `struct device` is `dev`.
fn with_registered(dev: *mut bindings::device, f: impl FnOnce(&Shared)) {
    for i in 0..MAX_DEVICES {
        if REGISTRY_DEV[i].load(ordering::Acquire) != dev as usize {
            continue;
        }
        let p = REGISTRY[i].load(ordering::Acquire);
        if !p.is_null() {
            // SAFETY: The slot belongs to `dev`, whose device lock the PM core
            // holds for this callback; unbind (which empties the slot) needs the
            // same lock, so the reference stays valid.
            f(unsafe { &*p });
        }
        return;
    }
}

unsafe extern "C" fn pm_suspend(dev: *mut bindings::device) -> core::ffi::c_int {
    with_registered(dev, Shared::suspend);
    0
}

unsafe extern "C" fn pm_resume(dev: *mut bindings::device) -> core::ffi::c_int {
    with_registered(dev, Shared::resume);
    0
}

/// System sleep callbacks (suspend-to-RAM/idle and hibernation).
static PM_OPS: sys::SyncStatic<bindings::dev_pm_ops> = sys::SyncStatic({
    let mut ops: bindings::dev_pm_ops = pin_init::zeroed();
    ops.suspend = Some(pm_suspend);
    ops.resume = Some(pm_resume);
    ops.freeze = Some(pm_suspend);
    ops.thaw = Some(pm_resume);
    ops.poweroff = Some(pm_suspend);
    ops.restore = Some(pm_resume);
    ops
});

/// A device index (`N` in `apex_N`), returned to the pool on drop.
struct DeviceIndex(u32);

impl DeviceIndex {
    fn alloc() -> Result<Self> {
        let mut cur = INDICES.load(ordering::Relaxed);
        loop {
            let free = (!cur).trailing_zeros();
            if free as usize >= MAX_DEVICES {
                return Err(EBUSY);
            }
            match INDICES.cmpxchg(cur, cur | 1 << free, ordering::Full) {
                Ok(_) => return Ok(Self(free)),
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Drop for DeviceIndex {
    fn drop(&mut self) {
        let mut cur = INDICES.load(ordering::Relaxed);
        while let Err(actual) = INDICES.cmpxchg(cur, cur & !(1 << self.0), ordering::Full) {
            cur = actual;
        }
    }
}

/// The misc device and the state its file operations reach.
#[pin_data]
pub(crate) struct MiscHost {
    shared: Arc<Shared>,
    #[pin]
    misc: MiscDeviceRegistration<ApexFile>,
}

impl MiscHost {
    /// The shared state of the device `misc` belongs to.
    pub(crate) fn shared_of(misc: &MiscDeviceRegistration<ApexFile>) -> Arc<Shared> {
        let misc = core::ptr::from_ref(misc);
        // SAFETY: Every `MiscDeviceRegistration<ApexFile>` is the `misc` field of
        // a pinned `MiscHost` (this module creates no others), and the
        // registration is alive while its `open` callback runs (`misc_open()`
        // holds `misc_mtx`, which `misc_deregister()` also takes).
        let host = unsafe { &*kernel::container_of!(misc, MiscHost, misc) };
        host.shared.clone()
    }
}

/// One MSI-X vector's handler.
struct IrqHandler {
    shared: Arc<Shared>,
    vector: usize,
}

impl irq::Handler for IrqHandler {
    fn handle(&self) -> IrqReturn {
        // MSI-X vectors are exclusive, so the interrupt is always ours.
        self.shared.interrupt(self.vector);
        IrqReturn::Handled
    }
}

/// Per-device driver data. Fields drop in declaration order, which is the
/// reverse of the order they are brought up in `probe()`.
#[pin_data]
struct ApexData<'bound> {
    /// `/dev/apex_N` disappears first.
    #[pin]
    host: MiscHost,
    _class_dev: sys::ClassDevice<Shared>,
    _hwmon: sys::HwmonDevice<Shared>,
    /// Must drop before `vectors`, which they borrow.
    _irqs: KVec<Pin<KBox<irq::Registration<'bound, IrqHandler>>>>,
    vectors: pci::IrqVectorRegistration<'bound>,
    shared: Arc<Shared>,
    _index: DeviceIndex,
}

struct ApexDriver;

kernel::pci_device_table!(
    PCI_TABLE,
    <ApexDriver as pci::Driver>::IdInfo,
    [(
        sys::pci_device_id(regs::PCI_VENDOR_ID, regs::PCI_DEVICE_ID),
        ()
    )]
);

fn params() -> Params {
    Params {
        allow_power_save: module_parameters::allow_power_save.value(),
        allow_sw_clock_gating: module_parameters::allow_sw_clock_gating.value(),
        allow_hw_clock_gating: module_parameters::allow_hw_clock_gating.value(),
        temp_poll_interval_ms: module_parameters::temp_poll_interval
            .value()
            .clamp(0, 3_600_000) as u32,
        test_resume_state_loss: module_parameters::test_resume_state_loss.value(),
    }
}

impl pci::Driver for ApexDriver {
    type IdInfo = ();
    type Data<'bound> = ApexData<'bound>;
    const ID_TABLE: pci::IdTable<Self::IdInfo> = &PCI_TABLE;

    fn probe<'bound>(
        pdev: &'bound pci::Device<Core<'_>>,
        _info: Option<&'bound Self::IdInfo>,
    ) -> impl PinInit<Self::Data<'bound>, Error> + 'bound {
        // Fields are brought up in the order written here; on failure the ones
        // already initialised are dropped again.
        try_pin_init!(ApexData {
            _index: DeviceIndex::alloc()?,
            shared: {
                pdev.enable_device_mem()?;
                pdev.set_master();
                // SAFETY: No DMA mappings or allocations exist for this device yet.
                unsafe { dma::Device::dma_set_mask_and_coherent(pdev, DmaMask::new::<64>())? };
                // The chip translates every 4 KiB page through its own table, so
                // it has no DMA segment limit. Without this the DMA layer assumes
                // 64 KiB and scatter-gather mappings of larger runs violate it.
                // SAFETY: As above, nothing is mapped yet.
                unsafe { dma::Device::dma_set_max_seg_size(pdev, !(sys::PAGE as u32 - 1)) };
                let bar = pdev
                    .iomap_region_sized::<{ regs::CSR_BAR_BYTES }>(regs::CSR_BAR, c"apex")?
                    .into_devres()?;
                let bound: &pci::Device<Bound> = pdev;
                Arc::pin_init(Shared::new(bound, _index.0, params(), bar)?, GFP_KERNEL)?
            },
            vectors: pdev.alloc_irq_vectors(
                regs::NUM_INTERRUPTS as u32,
                regs::NUM_INTERRUPTS as u32,
                pci::IrqTypes::default().with(pci::IrqType::MsiX),
            )?,
            _irqs: {
                // SAFETY: `vectors` is initialised and lives in the pinned driver
                // data until after `_irqs` is dropped (fields drop in declaration
                // order), so this borrow outlives every registration made from it.
                let vectors: &'bound pci::IrqVectorRegistration<'bound> =
                    unsafe { &*core::ptr::from_ref(&*vectors) };
                let name = NAMES[_index.0 as usize];
                let mut irqs = KVec::with_capacity(regs::NUM_INTERRUPTS, GFP_KERNEL)?;
                for vector in 0..regs::NUM_INTERRUPTS {
                    let handler = IrqHandler { shared: shared.clone(), vector };
                    // SAFETY: The registration is kept in `_irqs` and dropped
                    // normally; it is never leaked.
                    let reg = unsafe {
                        irq::Registration::new(vectors.index(vector)?.into(), irq::Flags::TRIGGER_NONE, name, Ok::<_, Error>(handler))
                    };
                    irqs.push(KBox::pin_init(reg, GFP_KERNEL)?, GFP_KERNEL)?;
                }
                // Vectors are wired; bring the chip to its idle state.
                shared.probe_init()?;
                irqs
            },
            _class_dev: {
                let bound: &pci::Device<Bound> = pdev;
                sys::ClassDevice::create(&sysfs::CLASS, bound.as_ref(), c"apex_%u", _index.0, shared.clone())?
            },
            _hwmon: {
                let bound: &pci::Device<Bound> = pdev;
                sys::HwmonDevice::register(bound.as_ref(), c"apex", &sysfs::HWMON_GROUPS, shared.clone())?
            },
            host <- {
                let shared = shared.clone();
                let name = NAMES[_index.0 as usize];
                try_pin_init!(MiscHost {
                    shared,
                    misc <- MiscDeviceRegistration::register(MiscDeviceOptions { name }),
                }? Error)
            },
            _: {
                sys::install_pm_ops(pdev, &PM_OPS);
                registry_add(_index.0, shared);
                shared.start_thermal_poll();
                dev_info!(pdev, "apex_{} ready (apex-rs {})\n", _index.0, DRIVER_VERSION);
            },
        }? Error)
    }

    fn unbind<'bound>(_pdev: &'bound pci::Device<Core<'_>>, this: Pin<&Self::Data<'bound>>) {
        registry_remove(this.shared.index);
        // Release everything that needs the device while it is still bound.
        this.shared.shutdown();
    }
}

/// The module: the `apex` class and the PCI driver.
#[pin_data]
struct ApexModule {
    /// Unregistered first, so no class device outlives the class.
    #[pin]
    _driver: driver::Registration<pci::Adapter<ApexDriver>>,
    _class: sys::ClassRegistration,
}

impl kernel::InPlaceModule for ApexModule {
    fn init(module: &'static ThisModule) -> impl PinInit<Self, Error> {
        try_pin_init!(Self {
            _class: sys::ClassRegistration::register(&sysfs::CLASS)?,
            _driver <- driver::Registration::new(c"apex", module),
        })
    }
}

module! {
    type: ApexModule,
    name: "apex",
    authors: ["apex-rs contributors"],
    description: "Google Coral Edge TPU (Apex) driver, gasket ABI compatible",
    license: "GPL v2",
    imports_ns: ["DMA_BUF"],
    params: {
        allow_power_save: bool {
            default: true,
            description: "Hold the TPU in low-power reset while the device is closed",
        },
        allow_sw_clock_gating: bool {
            default: false,
            description: "Honour APEX_IOCTL_GATE_CLOCK",
        },
        allow_hw_clock_gating: bool {
            default: true,
            description: "Let the hardware gate clocks when idle",
        },
        temp_poll_interval: i32 {
            default: 5000,
            description: "Temperature poll interval in ms for thermal throttling (0 disables)",
        },
        test_resume_state_loss: bool {
            default: false,
            description: "Testing only: handle every resume as if the TPU lost its state",
        },
    },
}
