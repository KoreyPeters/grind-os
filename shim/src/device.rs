// SPDX-License-Identifier: AGPL-3.0-or-later

//! Opening a PCI device through VFIO.

use crate::deny::{self, DenyList};
use crate::dma::{DmaAllocator, Domain};
use crate::log::{emit, Value};
use crate::sys::*;
use crate::window::Window;
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::Arc;

/// Why a device could not be opened: which step, what was expected, what happened.
#[derive(Debug)]
pub struct OpenError {
    pub step: &'static str,
    pub expected: String,
    pub got: String,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "step {}: expected {}, got {}",
            self.step, self.expected, self.got
        )
    }
}

fn fail(step: &'static str, expected: impl Into<String>, got: impl fmt::Display) -> OpenError {
    OpenError {
        step,
        expected: expected.into(),
        got: got.to_string(),
    }
}

/// For `map_err`.
fn fails<'a, E: fmt::Display>(
    step: &'static str,
    expected: &'a str,
) -> impl FnOnce(E) -> OpenError + 'a {
    move |e| fail(step, expected, e)
}

fn open_rw(path: &str) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

fn sysfs_hex(path: &str) -> Result<u16, OpenError> {
    let exp = format!("{path} holds a hex ID");
    let text = fs::read_to_string(path).map_err(fails("ids", &exp))?;
    let t = text.trim();
    u16::from_str_radix(t.trim_start_matches("0x"), 16).map_err(|_| fail("ids", exp.clone(), t))
}

fn region_info(device: &File, index: u32, step: &'static str) -> Result<RegionInfo, OpenError> {
    let mut info = RegionInfo {
        argsz: argsz::<RegionInfo>(),
        index,
        ..Default::default()
    };
    ioctl_ptr(device, VFIO_DEVICE_GET_REGION_INFO, &mut info)
        .map_err(fails(step, "VFIO_DEVICE_GET_REGION_INFO succeeds"))?;
    Ok(info)
}

/// One PCI device, opened through VFIO, as the shim holds it.
pub struct Device {
    bdf: String,
    vendor: u16,
    device_id: u16,
    deny: &'static DenyList,
    window: Window<File>,
    dma: DmaAllocator,
}

impl Device {
    /// Open the device at PCI address `bdf` (e.g. `0000:00:01.0`), which must
    /// already be bound to vfio-pci.
    ///
    /// Refuses devices with no deny list, and devices whose deny list has not
    /// been reviewed unless `allow_unreviewed` (for emulated devices only).
    /// DMA allocations are limited to `dma_limit` bytes in total.
    pub fn open(bdf: &str, allow_unreviewed: bool, dma_limit: u64) -> Result<Device, OpenError> {
        let sys = format!("/sys/bus/pci/devices/{bdf}");
        let vendor = sysfs_hex(&format!("{sys}/vendor"))?;
        let device_id = sysfs_hex(&format!("{sys}/device"))?;

        // Rule 10: no reviewed deny list, no access.
        let deny = deny::admit(vendor, device_id, allow_unreviewed)
            .map_err(|r| fail("deny-list", r.expected, r.got))?;

        // Container and group.
        let link = format!("{sys}/iommu_group");
        let exp = format!("{link} links to an IOMMU group");
        let target = fs::read_link(&link).map_err(fails("group", &exp))?;
        let group_id = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let container =
            open_rw("/dev/vfio/vfio").map_err(fails("container", "open /dev/vfio/vfio"))?;
        let version = ioctl_val(&container, VFIO_GET_API_VERSION, 0)
            .map_err(fails("container", "VFIO_GET_API_VERSION succeeds"))?;
        if version != VFIO_API_VERSION {
            return Err(fail(
                "container",
                format!("VFIO API version {VFIO_API_VERSION}"),
                version,
            ));
        }
        let ext = ioctl_val(&container, VFIO_CHECK_EXTENSION, VFIO_TYPE1V2_IOMMU)
            .map_err(fails("container", "VFIO_CHECK_EXTENSION succeeds"))?;
        if ext != 1 {
            return Err(fail(
                "container",
                "Type1 v2 IOMMU support",
                format!("VFIO_CHECK_EXTENSION returned {ext}"),
            ));
        }

        let group_path = format!("/dev/vfio/{group_id}");
        let group = open_rw(&group_path).map_err(fails("group", &group_path))?;
        let mut status = GroupStatus {
            argsz: argsz::<GroupStatus>(),
            ..Default::default()
        };
        ioctl_ptr(&group, VFIO_GROUP_GET_STATUS, &mut status)
            .map_err(fails("group", "VFIO_GROUP_GET_STATUS succeeds"))?;
        if status.flags & VFIO_GROUP_FLAGS_VIABLE == 0 {
            return Err(fail(
                "group",
                "a viable group (all devices bound to vfio-pci or no driver)",
                format!("flags 0x{:x}", status.flags),
            ));
        }
        let mut container_fd = container.as_raw_fd();
        ioctl_ptr(&group, VFIO_GROUP_SET_CONTAINER, &mut container_fd)
            .map_err(fails("group", "VFIO_GROUP_SET_CONTAINER succeeds"))?;
        ioctl_val(&container, VFIO_SET_IOMMU, VFIO_TYPE1V2_IOMMU)
            .map_err(fails("container", "VFIO_SET_IOMMU Type1 v2 succeeds"))?;

        // Device.
        let name = CString::new(bdf).map_err(fails("device", "a PCI address without NUL bytes"))?;
        let fd = ioctl_str(&group, VFIO_GROUP_GET_DEVICE_FD, &name)
            .map_err(fails("device", "VFIO_GROUP_GET_DEVICE_FD succeeds"))?;
        // SAFETY: the kernel just returned `fd` as a new descriptor owned by us.
        let dev = unsafe { File::from_raw_fd(fd) };
        let mut info = DeviceInfo {
            argsz: argsz::<DeviceInfo>(),
            ..Default::default()
        };
        ioctl_ptr(&dev, VFIO_DEVICE_GET_INFO, &mut info)
            .map_err(fails("device", "VFIO_DEVICE_GET_INFO succeeds"))?;
        if info.flags & VFIO_DEVICE_FLAGS_PCI == 0 {
            return Err(fail(
                "device",
                "a PCI device",
                format!("flags 0x{:x}", info.flags),
            ));
        }

        // Config space: confirm identity, then enable memory decode and bus
        // mastering (DMA). Drivers never see config space.
        let cfg = region_info(&dev, VFIO_PCI_CONFIG_REGION_INDEX, "config")?;
        let mut id = [0u8; 4];
        dev.read_exact_at(&mut id, cfg.offset)
            .map_err(fails("config", "read config dword 0"))?;
        let id = u32::from_le_bytes(id);
        let want = (u32::from(device_id) << 16) | u32::from(vendor);
        if id != want {
            return Err(fail(
                "config",
                format!("config dword 0 = 0x{want:08x}"),
                format!("0x{id:08x}"),
            ));
        }
        let mut cmd = [0u8; 2];
        dev.read_exact_at(&mut cmd, cfg.offset + PCI_COMMAND)
            .map_err(fails("config", "read PCI command register"))?;
        let cmd = u16::from_le_bytes(cmd) | PCI_COMMAND_MEMORY | PCI_COMMAND_MASTER;
        dev.write_all_at(&cmd.to_le_bytes(), cfg.offset + PCI_COMMAND)
            .map_err(fails("config", "write PCI command register"))?;

        // BAR0: the register window.
        let bar0 = region_info(&dev, VFIO_PCI_BAR0_REGION_INDEX, "bar0")?;
        let rw = VFIO_REGION_INFO_FLAG_READ | VFIO_REGION_INFO_FLAG_WRITE;
        if bar0.flags & rw != rw || bar0.size == 0 || bar0.size > u64::from(u32::MAX) {
            return Err(fail(
                "bar0",
                "a readable, writable BAR0 of 1 byte to 4 GiB",
                format!("flags 0x{:x}, size 0x{:x}", bar0.flags, bar0.size),
            ));
        }

        emit(
            "device-open",
            &[
                ("bdf", Value::Str(bdf)),
                ("vendor", Value::Hex(vendor.into())),
                ("device", Value::Hex(device_id.into())),
                ("model", Value::Str(deny.model)),
                ("iommu_group", Value::Str(&group_id)),
                ("bar0_size", Value::Hex(bar0.size)),
                ("deny_entries", Value::U64(deny.entries.len() as u64)),
                ("deny_reviewed", Value::Bool(deny.reviewed)),
                ("dma_limit", Value::Hex(dma_limit)),
            ],
        );

        let window = Window::new(dev, bar0.offset, bar0.size as u32, deny);
        let dma = DmaAllocator::new(
            Arc::new(Domain {
                container,
                _group: group,
            }),
            dma_limit,
        );
        Ok(Device {
            bdf: bdf.to_string(),
            vendor,
            device_id,
            deny,
            window,
            dma,
        })
    }

    pub fn bdf(&self) -> &str {
        &self.bdf
    }

    pub fn ids(&self) -> (u16, u16) {
        (self.vendor, self.device_id)
    }

    pub fn deny_list(&self) -> &'static DenyList {
        self.deny
    }

    pub fn window(&self) -> &Window<File> {
        &self.window
    }

    pub fn dma(&self) -> &DmaAllocator {
        &self.dma
    }
}
