// SPDX-License-Identifier: AGPL-3.0-or-later

//! VFIO userspace API: ioctl numbers and struct layouts.
//!
//! From the Linux userspace API header `<linux/vfio.h>` (GPL-2.0 WITH
//! Linux-syscall-note, which permits use by userspace). Values were checked
//! by compiling a C program against the header and printing them; see
//! shim/README.md.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::raw::{c_char, c_int, c_ulong, c_void};
use std::os::unix::io::AsRawFd;

// Each request is _IO(VFIO_TYPE = ';', VFIO_BASE + n).
pub const VFIO_GET_API_VERSION: c_ulong = 0x3b64;
pub const VFIO_CHECK_EXTENSION: c_ulong = 0x3b65;
pub const VFIO_SET_IOMMU: c_ulong = 0x3b66;
pub const VFIO_GROUP_GET_STATUS: c_ulong = 0x3b67;
pub const VFIO_GROUP_SET_CONTAINER: c_ulong = 0x3b68;
pub const VFIO_GROUP_GET_DEVICE_FD: c_ulong = 0x3b6a;
pub const VFIO_DEVICE_GET_INFO: c_ulong = 0x3b6b;
pub const VFIO_DEVICE_GET_REGION_INFO: c_ulong = 0x3b6c;
pub const VFIO_DEVICE_RESET: c_ulong = 0x3b6f;
pub const VFIO_IOMMU_GET_INFO: c_ulong = 0x3b70;
pub const VFIO_IOMMU_MAP_DMA: c_ulong = 0x3b71;
pub const VFIO_IOMMU_UNMAP_DMA: c_ulong = 0x3b72;

pub const VFIO_API_VERSION: c_int = 0;
pub const VFIO_TYPE1V2_IOMMU: c_ulong = 3;
pub const VFIO_GROUP_FLAGS_VIABLE: u32 = 1 << 0;
pub const VFIO_DEVICE_FLAGS_RESET: u32 = 1 << 0;
pub const VFIO_DEVICE_FLAGS_PCI: u32 = 1 << 1;
pub const VFIO_REGION_INFO_FLAG_READ: u32 = 1 << 0;
pub const VFIO_REGION_INFO_FLAG_WRITE: u32 = 1 << 1;
pub const VFIO_PCI_BAR0_REGION_INDEX: u32 = 0;
pub const VFIO_PCI_CONFIG_REGION_INDEX: u32 = 7;
pub const VFIO_DMA_MAP_FLAG_READ: u32 = 1 << 0;
pub const VFIO_DMA_MAP_FLAG_WRITE: u32 = 1 << 1;
/// `vfio_iommu_type1_info.flags`: the reply has a capability chain.
pub const VFIO_IOMMU_INFO_CAPS: u32 = 1 << 1;
/// Capability: the valid IOVA ranges, excluding reserved regions.
pub const VFIO_IOMMU_TYPE1_INFO_CAP_IOVA_RANGE: u16 = 1;

// <linux/pci_regs.h>
pub const PCI_COMMAND: u64 = 0x04;
/// The first BAR register in config space; BAR n is at 0x10 + 4n.
pub const PCI_BASE_ADDRESS_0: u64 = 0x10;
pub const PCI_COMMAND_MEMORY: u16 = 0x2;
pub const PCI_COMMAND_MASTER: u16 = 0x4;

#[repr(C)]
#[derive(Default)]
pub struct GroupStatus {
    pub argsz: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct DeviceInfo {
    pub argsz: u32,
    pub flags: u32,
    pub num_regions: u32,
    pub num_irqs: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct RegionInfo {
    pub argsz: u32,
    pub flags: u32,
    pub index: u32,
    pub cap_offset: u32,
    pub size: u64,
    pub offset: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct DmaMap {
    pub argsz: u32,
    pub flags: u32,
    pub vaddr: u64,
    pub iova: u64,
    pub size: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct DmaUnmap {
    pub argsz: u32,
    pub flags: u32,
    pub iova: u64,
    pub size: u64,
}

/// `struct vfio_iommu_type1_info` followed by room for its capability
/// chain. Layout: argsz u32 @0, flags u32 @4, iova_pgsizes u64 @8,
/// cap_offset u32 @16 (24 bytes); each capability starts with id u16,
/// version u16, next u32; the IOVA-range capability then has nr_iovas u32
/// @8 and ranges of {start u64, end u64 inclusive} from @16.
#[repr(C, align(8))]
pub struct IommuInfoBuf(pub [u8; 4096]);

/// `VFIO_IOMMU_GET_INFO` on a container with an IOMMU set: the raw reply,
/// for `dma::parse_iova_ranges`.
pub fn iommu_info(container: &File) -> io::Result<Vec<u8>> {
    let mut buf = IommuInfoBuf([0; 4096]);
    buf.0[..4].copy_from_slice(&4096u32.to_le_bytes());
    ioctl_ptr(container, VFIO_IOMMU_GET_INFO, &mut buf)?;
    Ok(buf.0.to_vec())
}

/// `argsz` for a struct: its size in bytes.
pub fn argsz<T>() -> u32 {
    std::mem::size_of::<T>() as u32
}

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

fn check(r: c_int) -> io::Result<c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// ioctl whose argument is an integer.
pub fn ioctl_val(f: &File, request: c_ulong, arg: c_ulong) -> io::Result<c_int> {
    // SAFETY: the request takes an integer argument, not a pointer.
    check(unsafe { ioctl(f.as_raw_fd(), request, arg) })
}

/// ioctl whose argument points to a `#[repr(C)]` value the kernel reads or fills in.
pub fn ioctl_ptr<T>(f: &File, request: c_ulong, arg: &mut T) -> io::Result<c_int> {
    // SAFETY: `arg` is a live, exclusively borrowed value whose layout matches
    // the struct this request expects (checked against <linux/vfio.h>).
    check(unsafe { ioctl(f.as_raw_fd(), request, arg as *mut T as *mut c_void) })
}

/// ioctl whose argument is a NUL-terminated string.
pub fn ioctl_str(f: &File, request: c_ulong, arg: &CString) -> io::Result<c_int> {
    // SAFETY: `arg` is a valid NUL-terminated string that outlives the call.
    check(unsafe { ioctl(f.as_raw_fd(), request, arg.as_ptr() as *const c_char) })
}
