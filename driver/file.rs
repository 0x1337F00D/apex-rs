// SPDX-License-Identifier: GPL-2.0-only

//! `/dev/apex_N`: open, release, mmap and the gasket/apex ioctls.

use crate::{
    abi::{self, Direction},
    device::Shared,
    sys, MiscHost,
};
use core::mem::size_of;
use kernel::{
    fs::{file, File},
    miscdevice::{MiscDevice, MiscDeviceRegistration},
    mm::virt::VmaNew,
    prelude::*,
    sync::Arc,
    transmute::{AsBytes, FromBytes},
    uaccess::{UserPtr, UserSlice},
};

// SAFETY: Plain `repr(C)` integer structs without padding (the one implicit pad
// of `PageTableIoctlFlags` is an explicit field), valid for every bit pattern.
unsafe impl FromBytes for abi::InterruptEventfd {}
// SAFETY: See above.
unsafe impl FromBytes for abi::PageTableIoctl {}
// SAFETY: See above.
unsafe impl AsBytes for abi::PageTableIoctl {}
// SAFETY: See above.
unsafe impl FromBytes for abi::PageTableIoctlFlags {}
// SAFETY: See above.
unsafe impl FromBytes for abi::CoherentAllocConfig {}
// SAFETY: See above.
unsafe impl AsBytes for abi::CoherentAllocConfig {}
// SAFETY: See above.
unsafe impl FromBytes for abi::PageTableIoctlDmabuf {}
// SAFETY: See above.
unsafe impl FromBytes for abi::GateClock {}
// SAFETY: See above.
unsafe impl FromBytes for abi::PerformanceExpectation {}

/// One open file of an Apex device node.
pub(crate) struct ApexFile {
    shared: Arc<Shared>,
    writer: bool,
    mapping: usize,
}

fn read_user<T: FromBytes>(arg: usize) -> Result<T> {
    UserSlice::new(UserPtr::from_addr(arg), size_of::<T>())
        .reader()
        .read()
}

fn write_user<T: AsBytes>(arg: usize, value: &T) -> Result {
    UserSlice::new(UserPtr::from_addr(arg), size_of::<T>())
        .writer()
        .write(value)
}

impl ApexFile {
    fn ioctl(&self, cmd: u32, arg: usize) -> Result<isize> {
        use abi::*;
        let s = &*self.shared;
        // Permission rules of gasket_ioctl_check_permissions() and
        // apex_ioctl_check_permissions(): queries need a readable file,
        // everything else a writable one.
        let needs_write = !matches!(
            cmd,
            GASKET_IOCTL_PAGE_TABLE_SIZE
                | GASKET_IOCTL_SIMPLE_PAGE_TABLE_SIZE
                | GASKET_IOCTL_NUMBER_PAGE_TABLES
        );
        if needs_write && !self.writer {
            return Err(EPERM);
        }
        match cmd {
            GASKET_IOCTL_RESET => s.reset_ioctl()?,
            GASKET_IOCTL_SET_EVENTFD => {
                let req: InterruptEventfd = read_user(arg)?;
                s.set_eventfd(req.interrupt, Some(req.event_fd))?;
            }
            GASKET_IOCTL_CLEAR_EVENTFD | GASKET_IOCTL_CLEAR_EVENTFD_32 => {
                s.set_eventfd(arg as u64, None)?;
            }
            GASKET_IOCTL_NUMBER_PAGE_TABLES => write_user(arg, &1u64)?,
            GASKET_IOCTL_PAGE_TABLE_SIZE | GASKET_IOCTL_SIMPLE_PAGE_TABLE_SIZE => {
                let mut req: PageTableIoctl = read_user(arg)?;
                if req.page_table_index != 0 {
                    return Err(EINVAL);
                }
                let (total, simple, _) = s.page_table_info();
                req.size = if cmd == GASKET_IOCTL_PAGE_TABLE_SIZE {
                    total
                } else {
                    simple
                };
                write_user(arg, &req)?;
            }
            GASKET_IOCTL_PARTITION_PAGE_TABLE => {
                let req: PageTableIoctl = read_user(arg)?;
                if req.page_table_index != 0 {
                    return Err(EINVAL);
                }
                s.partition(req.size)?;
            }
            GASKET_IOCTL_MAP_BUFFER => {
                let req: PageTableIoctl = read_user(arg)?;
                self.map(&req, Direction::Bidirectional)?;
            }
            GASKET_IOCTL_MAP_BUFFER_FLAGS => {
                let req: PageTableIoctlFlags = read_user(arg)?;
                let dir = Direction::from_flags(req.flags).ok_or(EINVAL)?;
                self.map(&req.base, dir)?;
            }
            GASKET_IOCTL_MAP_BUFFER_FLAGS_32 => {
                // 36 bytes: the 32-byte base followed directly by the flags.
                let mut r =
                    UserSlice::new(UserPtr::from_addr(arg), size_of::<PageTableIoctl>() + 4)
                        .reader();
                let base: PageTableIoctl = r.read()?;
                let flags: u32 = r.read()?;
                let dir = Direction::from_flags(flags).ok_or(EINVAL)?;
                self.map(&base, dir)?;
            }
            GASKET_IOCTL_UNMAP_BUFFER => {
                let req: PageTableIoctl = read_user(arg)?;
                if req.page_table_index != 0 {
                    return Err(EINVAL);
                }
                s.unmap_buffer(req.device_address, req.size)?;
            }
            GASKET_IOCTL_CLEAR_INTERRUPT_COUNTS => s.clear_interrupt_counts(),
            GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR => {
                let mut req: CoherentAllocConfig = read_user(arg)?;
                if req.page_table_index != 0 {
                    return Err(EINVAL);
                }
                req.dma_address = s.config_coherent(req.enable != 0, req.size, req.dma_address)?;
                write_user(arg, &req)?;
            }
            // Only gasket's loopback test devices implement this.
            GASKET_IOCTL_LOOPBACK_INTERRUPT | GASKET_IOCTL_LOOPBACK_INTERRUPT_32 => {
                return Err(ENOTTY)
            }
            GASKET_IOCTL_MAP_DMABUF => {
                let req: PageTableIoctlDmabuf = read_user(arg)?;
                if req.page_table_index != 0 || req.num_pages == 0 {
                    return Err(EINVAL);
                }
                if req.map != 0 {
                    let dir = Direction::from_flags(req.flags).ok_or(EINVAL)?;
                    s.map_dmabuf(req.dmabuf_fd, req.device_address, req.num_pages, dir)?;
                } else {
                    s.unmap_dmabuf(req.dmabuf_fd, req.device_address, req.num_pages)?;
                }
            }
            APEX_IOCTL_GATE_CLOCK => {
                let req: GateClock = read_user(arg)?;
                s.gate_clock(req.enable != 0)?;
            }
            APEX_IOCTL_PERFORMANCE_EXPECTATION => {
                let req: PerformanceExpectation = read_user(arg)?;
                s.set_performance(req.performance)?;
            }
            _ => return Err(ENOTTY),
        }
        Ok(0)
    }

    fn map(&self, req: &abi::PageTableIoctl, dir: Direction) -> Result {
        if req.page_table_index != 0 {
            return Err(EINVAL);
        }
        self.shared
            .map_buffer(req.host_address, req.device_address, req.size, dir)
    }
}

#[vtable]
impl MiscDevice for ApexFile {
    type Ptr = KBox<Self>;

    fn open(file: &File, misc: &MiscDeviceRegistration<Self>) -> Result<KBox<Self>> {
        let shared = MiscHost::shared_of(misc);
        let writer = file.flags() & file::flags::O_ACCMODE != file::flags::O_RDONLY;
        let mapping = sys::file_mapping(file);
        let this = KBox::new(
            Self {
                shared,
                writer,
                mapping,
            },
            GFP_KERNEL,
        )?;
        this.shared.open(file, writer)?;
        Ok(this)
    }

    fn release(this: KBox<Self>, _file: &File) {
        this.shared.release(this.mapping, this.writer);
    }

    fn ioctl(this: &Self, _file: &File, cmd: u32, arg: usize) -> Result<isize> {
        this.ioctl(cmd, arg)
    }

    fn mmap(this: &Self, _file: &File, vma: &VmaNew) -> Result {
        this.shared.mmap(vma, this.writer)
    }
}
