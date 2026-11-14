// SPDX-License-Identifier: AGPL-3.0-or-later

//! Opening a PCI device through VFIO.

use crate::deny::{self, WriteList};
use crate::dma::{parse_iova_ranges, DmaAllocator, Domain, Regions};
use crate::log::{emit, Value};
use crate::nvme::{Nvme, NvmeError, COMPLETION_BYTES};
use crate::sys::*;
use crate::window::Window;
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
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
    deny: &'static WriteList,
    window: Window<File>,
    dma: DmaAllocator,
    /// For a device that takes commands (NVMe): the controller the shim
    /// brought up, with its admin and submission queues.
    nvme: Option<Nvme>,
    /// A second handle on the VFIO device fd, for config space.
    config: File,
    config_offset: u64,
    /// Set once bus mastering has been cleared.
    quiesced: AtomicBool,
}

fn read_command(config: &File, config_offset: u64) -> std::io::Result<u16> {
    let mut cmd = [0u8; 2];
    config.read_exact_at(&mut cmd, config_offset + PCI_COMMAND)?;
    Ok(u16::from_le_bytes(cmd))
}

impl Device {
    /// Open the device at PCI address `bdf` (e.g. `0000:00:01.0`), which must
    /// already be bound to vfio-pci.
    ///
    /// Refuses devices with no write list, and devices whose list has not
    /// been reviewed unless `allow_unreviewed` is set, the machine's DMI
    /// system vendor names an emulator, and the device's IDs and subsystem
    /// IDs are those of a device QEMU emulates (see `deny::admit`).
    /// DMA allocations are limited to `dma_limit` bytes in total.
    pub fn open(bdf: &str, allow_unreviewed: bool, dma_limit: u64) -> Result<Device, OpenError> {
        let sys = format!("/sys/bus/pci/devices/{bdf}");
        let vendor = sysfs_hex(&format!("{sys}/vendor"))?;
        let device_id = sysfs_hex(&format!("{sys}/device"))?;

        // Rule 10: no reviewed write list, no access.
        let sys_vendor = deny::sys_vendor();
        // Unreadable subsystem IDs are passed as None; the gate refuses an
        // unreviewed list then, and a reviewed one does not need them.
        let subsystem = sysfs_hex(&format!("{sys}/subsystem_vendor"))
            .ok()
            .zip(sysfs_hex(&format!("{sys}/subsystem_device")).ok());
        let deny = deny::admit(
            vendor,
            device_id,
            subsystem,
            allow_unreviewed,
            sys_vendor.as_deref(),
        )
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

        // Which IOVAs the IOMMU will map (reserved regions such as the MSI
        // window excluded), for the DMA allocator's regions. If the kernel
        // does not say, the allocator assumes the high region and offers no
        // low one.
        let ranges = match iommu_info(&container) {
            Ok(reply) => parse_iova_ranges(&reply),
            Err(e) => {
                emit(
                    "dma-ranges-error",
                    &[
                        ("attempted", Value::Str("VFIO_IOMMU_GET_INFO")),
                        ("error", Value::Str(&e.to_string())),
                    ],
                );
                None
            }
        };
        let regions = Regions::from_ranges(ranges.as_deref());
        let show = |r: Option<(u64, u64)>| {
            r.map_or("none".to_string(), |(s, e)| format!("0x{s:x}-0x{e:x}"))
        };
        emit(
            "dma-regions",
            &[
                (
                    "iova_ranges",
                    Value::Str(&ranges.as_ref().map_or("not reported".to_string(), |rs| {
                        rs.iter()
                            .map(|(s, e)| format!("0x{s:x}-0x{e:x}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })),
                ),
                ("high", Value::Str(&show(regions.high))),
                ("low", Value::Str(&show(regions.low))),
            ],
        );

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

        // Reset, so no state (rings, DMA addresses, enabled engines) carries
        // over from whoever used the device before. A device VFIO cannot
        // reset is refused rather than run in an unknown state.
        if info.flags & VFIO_DEVICE_FLAGS_RESET == 0 {
            return Err(fail(
                "reset",
                "a device VFIO can reset (VFIO_DEVICE_FLAGS_RESET)",
                format!("device info flags 0x{:x}", info.flags),
            ));
        }
        ioctl_val(&dev, VFIO_DEVICE_RESET, 0)
            .map_err(fails("reset", "VFIO_DEVICE_RESET succeeds"))?;

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
        let cmd = read_command(&dev, cfg.offset)
            .map_err(fails("config", "read PCI command register"))?
            | PCI_COMMAND_MEMORY
            | PCI_COMMAND_MASTER;
        dev.write_all_at(&cmd.to_le_bytes(), cfg.offset + PCI_COMMAND)
            .map_err(fails("config", "write PCI command register"))?;
        let config = dev
            .try_clone()
            .map_err(fails("config", "duplicate the device fd for config space"))?;

        // The register window: the memory BAR the write list names. An
        // I/O-port BAR is refused: its registers are reached indirectly and
        // the portals never expose one. Config space says which kind a BAR
        // is (bit 0 of the BAR register: 1 for I/O).
        let bar_index = deny.bar;
        if bar_index > 5 {
            return Err(fail("bar", "a write list naming BAR 0 to 5", bar_index));
        }
        let mut bar_reg = [0u8; 4];
        dev.read_exact_at(
            &mut bar_reg,
            cfg.offset + PCI_BASE_ADDRESS_0 + 4 * u64::from(bar_index),
        )
        .map_err(fails("bar", "read the BAR register in config space"))?;
        let bar_reg = u32::from_le_bytes(bar_reg);
        if bar_reg & 1 != 0 {
            return Err(fail(
                "bar",
                format!("BAR{bar_index} is a memory BAR"),
                format!("an I/O-port BAR (config 0x{bar_reg:08x})"),
            ));
        }
        let bar = region_info(&dev, VFIO_PCI_BAR0_REGION_INDEX + bar_index, "bar")?;
        let rw = VFIO_REGION_INFO_FLAG_READ | VFIO_REGION_INFO_FLAG_WRITE;
        if bar.flags & rw != rw || bar.size == 0 || bar.size > u64::from(u32::MAX) {
            return Err(fail(
                "bar",
                format!("a readable, writable BAR{bar_index} of 1 byte to 4 GiB"),
                format!("flags 0x{:x}, size 0x{:x}", bar.flags, bar.size),
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
                ("bar", Value::U64(bar_index.into())),
                ("bar_size", Value::Hex(bar.size)),
                ("list_kind", Value::Str(deny.kind.as_str())),
                ("deny_entries", Value::U64(deny.entries.len() as u64)),
                ("deny_reviewed", Value::Bool(deny.reviewed)),
                ("commands", Value::Bool(deny.commands.is_some())),
                (
                    "commands_reviewed",
                    Value::Bool(deny.commands.is_some_and(|c| c.reviewed)),
                ),
                (
                    "dmi_sys_vendor",
                    Value::Str(sys_vendor.as_deref().unwrap_or("unreadable")),
                ),
                (
                    "subsystem",
                    Value::Str(
                        &subsystem
                            .map(|(v, d)| format!("{v:04x}:{d:04x}"))
                            .unwrap_or_else(|| "unreadable".into()),
                    ),
                ),
                ("dma_limit", Value::Hex(dma_limit)),
                ("reset", Value::Str("VFIO_DEVICE_RESET ok")),
            ],
        );

        let window = Window::new(dev, bar.offset, bar.size as u32, deny);
        let dma = DmaAllocator::new(
            Arc::new(Domain {
                container,
                _group: group,
            }),
            dma_limit,
            regions,
        );
        // A device that fetches commands from memory: the shim configures
        // it and owns its queues (rule 10). Bus mastering is already on, so
        // a failure here quiesces before returning.
        let nvme = match deny.commands {
            None => None,
            Some(list) => match Nvme::bring_up(&window, &dma, list) {
                Ok(n) => Some(n),
                Err(e) => {
                    let cleared = cmd & !PCI_COMMAND_MASTER;
                    let _ = config.write_all_at(&cleared.to_le_bytes(), cfg.offset + PCI_COMMAND);
                    return Err(fail(e.step, e.expected, e.got));
                }
            },
        };
        Ok(Device {
            bdf: bdf.to_string(),
            vendor,
            device_id,
            deny,
            window,
            dma,
            nvme,
            config,
            config_offset: cfg.offset,
            quiesced: AtomicBool::new(false),
        })
    }

    /// Stop the device from issuing DMA: clear bus mastering in the PCI
    /// command register and check that it reads back clear. Call this
    /// before DMA buffers are unmapped. Runs once; later calls return Ok.
    /// Dropping the device calls it too, as a backstop.
    ///
    /// The outcome is logged (`device-quiesce` or `device-quiesce-error`).
    pub fn quiesce(&self) -> Result<(), String> {
        if self.quiesced.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let result = (|| {
            let before = read_command(&self.config, self.config_offset)
                .map_err(|e| format!("read PCI command register: {e}"))?;
            let cleared = before & !PCI_COMMAND_MASTER;
            self.config
                .write_all_at(&cleared.to_le_bytes(), self.config_offset + PCI_COMMAND)
                .map_err(|e| format!("write PCI command register: {e}"))?;
            let after = read_command(&self.config, self.config_offset)
                .map_err(|e| format!("read back PCI command register: {e}"))?;
            if after & PCI_COMMAND_MASTER != 0 {
                return Err(format!(
                    "PCI command register still has bus mastering set: 0x{before:04x} -> 0x{after:04x}"
                ));
            }
            Ok((before, after))
        })();
        match result {
            Ok((before, after)) => {
                emit(
                    "device-quiesce",
                    &[
                        ("bdf", Value::Str(&self.bdf)),
                        ("command_before", Value::Hex(before.into())),
                        ("command_after", Value::Hex(after.into())),
                        ("bus_master", Value::Str("off")),
                    ],
                );
                Ok(())
            }
            Err(e) => {
                emit(
                    "device-quiesce-error",
                    &[
                        ("bdf", Value::Str(&self.bdf)),
                        ("expected", Value::Str("bus mastering cleared")),
                        ("got", Value::Str(&e)),
                    ],
                );
                Err(e)
            }
        }
    }

    pub fn bdf(&self) -> &str {
        &self.bdf
    }

    pub fn ids(&self) -> (u16, u16) {
        (self.vendor, self.device_id)
    }

    pub fn deny_list(&self) -> &'static WriteList {
        self.deny
    }

    pub fn window(&self) -> &Window<File> {
        &self.window
    }

    pub fn dma(&self) -> &DmaAllocator {
        &self.dma
    }

    /// Whether the device takes commands through the shim (NVMe).
    pub fn takes_commands(&self) -> bool {
        self.nvme.is_some()
    }

    /// Commands the shim refused so far (rule 10). Any value above zero
    /// fails the run.
    pub fn refused_commands(&self) -> u32 {
        self.nvme.as_ref().map_or(0, Nvme::refused)
    }

    /// Run one admin command for the driver. `None` if the device takes no
    /// commands.
    pub fn nvme_admin(&self, command: &[u8]) -> Option<Result<[u8; COMPLETION_BYTES], NvmeError>> {
        let n = self.nvme.as_ref()?;
        Some(n.admin(&self.window, &self.dma, command))
    }

    /// Queue one I/O command for the driver. `None` if the device takes no
    /// commands.
    pub fn nvme_submit(&self, sqid: u16, command: &[u8]) -> Option<Result<(), NvmeError>> {
        let n = self.nvme.as_ref()?;
        Some(n.submit(&self.window, &self.dma, sqid, command))
    }

    /// Take the completions waiting in I/O completion queue `cqid`. `None`
    /// if the device takes no commands.
    pub fn nvme_complete(
        &self,
        cqid: u16,
    ) -> Option<Result<Vec<[u8; COMPLETION_BYTES]>, NvmeError>> {
        let n = self.nvme.as_ref()?;
        Some(n.complete(&self.window, cqid))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // Backstop for callers that never quiesced. A failure is already
        // logged by quiesce; there is no one left to return it to.
        let _ = self.quiesce();
    }
}
