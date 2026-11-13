// SPDX-License-Identifier: AGPL-3.0-or-later

//! Phase 0, milestone 2: open the e1000e through VFIO and read one register.
//!
//! Usage: `vfio-hello <pci-address>`, e.g. `vfio-hello 0000:00:01.0`.
//! The device must already be bound to vfio-pci (vm/init does this).
//!
//! Prints one line per step and stops at the first failure:
//!
//! ```text
//! VFIO step=<step> result=pass|fail expected="..." got="..."
//! ```
//!
//! Exits 0 only if every step passes.
//!
//! Sources: VFIO ioctl numbers and struct layouts are from the Linux
//! userspace API header `<linux/vfio.h>` (GPL-2.0 WITH Linux-syscall-note,
//! which permits use by userspace) and the kernel's VFIO documentation.
//! The register offset is from the Intel 82574 GbE Controller datasheet.

use std::ffi::CString;
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::raw::{c_char, c_int, c_ulong, c_void};
use std::os::unix::fs::FileExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::process::ExitCode;

// <linux/vfio.h>: each request is _IO(VFIO_TYPE = ';', VFIO_BASE + n).
const VFIO_GET_API_VERSION: c_ulong = 0x3b64;
const VFIO_CHECK_EXTENSION: c_ulong = 0x3b65;
const VFIO_SET_IOMMU: c_ulong = 0x3b66;
const VFIO_GROUP_GET_STATUS: c_ulong = 0x3b67;
const VFIO_GROUP_SET_CONTAINER: c_ulong = 0x3b68;
const VFIO_GROUP_GET_DEVICE_FD: c_ulong = 0x3b6a;
const VFIO_DEVICE_GET_INFO: c_ulong = 0x3b6b;
const VFIO_DEVICE_GET_REGION_INFO: c_ulong = 0x3b6c;

const VFIO_API_VERSION: c_int = 0;
const VFIO_TYPE1V2_IOMMU: c_ulong = 3;
const VFIO_GROUP_FLAGS_VIABLE: u32 = 1 << 0;
const VFIO_DEVICE_FLAGS_PCI: u32 = 1 << 1;
const VFIO_REGION_INFO_FLAG_READ: u32 = 1 << 0;
const VFIO_PCI_BAR0_REGION_INDEX: u32 = 0;
const VFIO_PCI_CONFIG_REGION_INDEX: u32 = 7;

/// PCI config dword 0 of the 82574L: device ID 0x10d3 << 16 | vendor ID 0x8086.
const E1000E_ID: u32 = 0x10d3_8086;
/// Device Status register (STATUS), offset 0x00008 in BAR0 (82574 datasheet).
/// Read-only, and reading it has no side effects.
const STATUS: u64 = 0x0_0008;

#[repr(C)]
#[derive(Default)]
struct GroupStatus {
    argsz: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct DeviceInfo {
    argsz: u32,
    flags: u32,
    num_regions: u32,
    num_irqs: u32,
}

#[repr(C)]
#[derive(Default)]
struct RegionInfo {
    argsz: u32,
    flags: u32,
    index: u32,
    cap_offset: u32,
    size: u64,
    offset: u64,
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
fn ioctl_val(f: &File, request: c_ulong, arg: c_ulong) -> io::Result<c_int> {
    // SAFETY: the request takes an integer argument, not a pointer.
    check(unsafe { ioctl(f.as_raw_fd(), request, arg) })
}

/// ioctl whose argument points to a `#[repr(C)]` value the kernel reads or fills in.
fn ioctl_ptr<T>(f: &File, request: c_ulong, arg: &mut T) -> io::Result<c_int> {
    // SAFETY: `arg` is a live, exclusively borrowed value whose layout matches
    // the struct this request expects (checked against <linux/vfio.h>).
    check(unsafe { ioctl(f.as_raw_fd(), request, arg as *mut T as *mut c_void) })
}

/// ioctl whose argument is a NUL-terminated string.
fn ioctl_str(f: &File, request: c_ulong, arg: &CString) -> io::Result<c_int> {
    // SAFETY: `arg` is a valid NUL-terminated string that outlives the call.
    check(unsafe { ioctl(f.as_raw_fd(), request, arg.as_ptr() as *const c_char) })
}

fn open_rw(path: &str) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

fn region_info(device: &File, index: u32) -> io::Result<RegionInfo> {
    let mut info = RegionInfo {
        argsz: std::mem::size_of::<RegionInfo>() as u32,
        index,
        ..Default::default()
    };
    ioctl_ptr(device, VFIO_DEVICE_GET_REGION_INFO, &mut info)?;
    Ok(info)
}

fn read_u32(device: &File, offset: u64) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    device.read_exact_at(&mut buf, offset)?;
    Ok(u32::from_le_bytes(buf))
}

/// Keep values on one line and free of double quotes so lines stay parseable.
fn clean(s: &str) -> String {
    s.replace(['\n', '"'], " ")
}

fn report(step: &str, result: &str, expected: &str, got: &str) {
    println!(
        "VFIO step={step} result={result} expected=\"{}\" got=\"{}\"",
        clean(expected),
        clean(got)
    );
}

struct Failure {
    step: &'static str,
    expected: String,
    got: String,
}

impl Failure {
    fn new(step: &'static str, expected: &str, got: impl Display) -> Self {
        Failure {
            step,
            expected: expected.to_string(),
            got: got.to_string(),
        }
    }
}

/// For `map_err`: turn an error into a failure of `step`.
fn fails<'a, E: Display>(step: &'static str, expected: &'a str) -> impl FnOnce(E) -> Failure + 'a {
    move |e| Failure::new(step, expected, e)
}

/// Pass `step` if `ok`, otherwise fail it; either way `got` is reported.
fn expect(step: &'static str, ok: bool, expected: &str, got: String) -> Result<(), Failure> {
    if ok {
        report(step, "pass", expected, &got);
        Ok(())
    } else {
        Err(Failure::new(step, expected, got))
    }
}

fn run(bdf: &str) -> Result<(), Failure> {
    // The device's IOMMU group, which names the VFIO group file.
    let link = format!("/sys/bus/pci/devices/{bdf}/iommu_group");
    let exp = format!("{link} links to an IOMMU group");
    let target = fs::read_link(&link).map_err(fails("group", &exp))?;
    let group = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    expect("group", !group.is_empty(), &exp, format!("group {group}"))?;

    // Container: the IOMMU context the device's DMA will be confined to.
    let exp = "open /dev/vfio/vfio read-write";
    let container = open_rw("/dev/vfio/vfio").map_err(fails("open-container", exp))?;
    expect("open-container", true, exp, "opened".into())?;

    let exp = format!("VFIO API version {VFIO_API_VERSION}");
    let version =
        ioctl_val(&container, VFIO_GET_API_VERSION, 0).map_err(fails("api-version", &exp))?;
    expect(
        "api-version",
        version == VFIO_API_VERSION,
        &exp,
        format!("version {version}"),
    )?;

    let exp = "container supports the Type1 v2 IOMMU";
    let ext = ioctl_val(&container, VFIO_CHECK_EXTENSION, VFIO_TYPE1V2_IOMMU)
        .map_err(fails("type1v2", exp))?;
    expect(
        "type1v2",
        ext == 1,
        exp,
        format!("VFIO_CHECK_EXTENSION returned {ext}"),
    )?;

    // Group: every device in it must be bound to vfio-pci (or unbound).
    let path = format!("/dev/vfio/{group}");
    let exp = format!("open {path} read-write");
    let group_file = open_rw(&path).map_err(fails("open-group", &exp))?;
    expect("open-group", true, &exp, "opened".into())?;

    let exp = "group is viable (all its devices bound to vfio-pci or no driver)";
    let mut status = GroupStatus {
        argsz: std::mem::size_of::<GroupStatus>() as u32,
        ..Default::default()
    };
    ioctl_ptr(&group_file, VFIO_GROUP_GET_STATUS, &mut status)
        .map_err(fails("group-viable", exp))?;
    expect(
        "group-viable",
        status.flags & VFIO_GROUP_FLAGS_VIABLE != 0,
        exp,
        format!("flags 0x{:x}", status.flags),
    )?;

    let exp = "VFIO_GROUP_SET_CONTAINER succeeds";
    let mut container_fd: c_int = container.as_raw_fd();
    ioctl_ptr(&group_file, VFIO_GROUP_SET_CONTAINER, &mut container_fd)
        .map_err(fails("set-container", exp))?;
    expect("set-container", true, exp, "ok".into())?;

    let exp = "VFIO_SET_IOMMU Type1 v2 succeeds";
    ioctl_val(&container, VFIO_SET_IOMMU, VFIO_TYPE1V2_IOMMU).map_err(fails("set-iommu", exp))?;
    expect("set-iommu", true, exp, "ok".into())?;

    // Device.
    let exp = format!("VFIO_GROUP_GET_DEVICE_FD {bdf} returns a file descriptor");
    let name = CString::new(bdf).map_err(fails("device-fd", &exp))?;
    let fd = ioctl_str(&group_file, VFIO_GROUP_GET_DEVICE_FD, &name)
        .map_err(fails("device-fd", &exp))?;
    // SAFETY: the kernel just returned `fd` as a new descriptor owned by us.
    let device = unsafe { File::from_raw_fd(fd) };
    expect("device-fd", true, &exp, format!("fd {fd}"))?;

    let exp = "a PCI device";
    let mut info = DeviceInfo {
        argsz: std::mem::size_of::<DeviceInfo>() as u32,
        ..Default::default()
    };
    ioctl_ptr(&device, VFIO_DEVICE_GET_INFO, &mut info).map_err(fails("device-info", exp))?;
    expect(
        "device-info",
        info.flags & VFIO_DEVICE_FLAGS_PCI != 0,
        exp,
        format!(
            "flags 0x{:x}, {} regions, {} irqs",
            info.flags, info.num_regions, info.num_irqs
        ),
    )?;

    // Cross-check through config space that this is the device we think it is.
    let exp = "readable config space region";
    let cfg =
        region_info(&device, VFIO_PCI_CONFIG_REGION_INDEX).map_err(fails("config-region", exp))?;
    expect(
        "config-region",
        cfg.flags & VFIO_REGION_INFO_FLAG_READ != 0 && cfg.size >= 4,
        exp,
        format!(
            "flags 0x{:x}, size 0x{:x}, offset 0x{:x}",
            cfg.flags, cfg.size, cfg.offset
        ),
    )?;

    let exp = format!("config dword 0 = 0x{E1000E_ID:08x} (82574L)");
    let id = read_u32(&device, cfg.offset).map_err(fails("config-id", &exp))?;
    expect("config-id", id == E1000E_ID, &exp, format!("0x{id:08x}"))?;

    // The register read.
    let exp = format!("readable BAR0 region covering STATUS at 0x{STATUS:05x}");
    let bar0 =
        region_info(&device, VFIO_PCI_BAR0_REGION_INDEX).map_err(fails("bar0-region", &exp))?;
    expect(
        "bar0-region",
        bar0.flags & VFIO_REGION_INFO_FLAG_READ != 0 && bar0.size >= STATUS + 4,
        &exp,
        format!(
            "flags 0x{:x}, size 0x{:x}, offset 0x{:x}",
            bar0.flags, bar0.size, bar0.offset
        ),
    )?;

    let exp =
        "STATUS reads back something other than 0xffffffff (all ones: the device did not respond)";
    let value = read_u32(&device, bar0.offset + STATUS).map_err(fails("read-status", exp))?;
    expect(
        "read-status",
        value != u32::MAX,
        exp,
        format!("STATUS = 0x{value:08x}"),
    )?;

    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        report(
            "args",
            "fail",
            "one argument: the PCI address, e.g. 0000:00:01.0",
            &args[1..].join(" "),
        );
        return ExitCode::FAILURE;
    }
    match run(&args[1]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            report(f.step, "fail", &f.expected, &f.got);
            ExitCode::FAILURE
        }
    }
}
