// SPDX-License-Identifier: AGPL-3.0-or-later

//! The register window: checked 32-bit access to one device region.

use crate::deny::{WriteList, WriteRefusal};
use crate::log::{emit, Value};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, Ordering};

/// Positioned reads and writes on a device region. Implemented by the VFIO
/// device file; tests use an in-memory fake.
pub trait RegionIo {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
}

impl RegionIo for File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        FileExt::write_all_at(self, buf, offset)
    }
}

#[derive(Debug)]
pub enum MmioError {
    OutOfRange,
    Misaligned,
    /// The device's write list refused the write (rule 10). Already logged
    /// and counted.
    Denied(WriteRefusal),
    /// The access itself failed. Not the driver's fault; already logged.
    Io(io::Error),
}

/// A device region (BAR0) as drivers see it.
pub struct Window<R: RegionIo> {
    io: R,
    /// Offset of the region within `io` (VFIO's region offset).
    base: u64,
    size: u32,
    deny: &'static WriteList,
    violations: AtomicU32,
}

impl<R: RegionIo> Window<R> {
    pub fn new(io: R, base: u64, size: u32, deny: &'static WriteList) -> Self {
        Window {
            io,
            base,
            size,
            deny,
            violations: AtomicU32::new(0),
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    /// Number of denied writes so far. Any value above zero fails the run.
    pub fn violations(&self) -> u32 {
        self.violations.load(Ordering::SeqCst)
    }

    /// Natural alignment, and inside the window.
    fn check(&self, offset: u32, width: u32) -> Result<(), MmioError> {
        if !offset.is_multiple_of(width) {
            return Err(MmioError::Misaligned);
        }
        if u64::from(offset) + u64::from(width) > u64::from(self.size) {
            return Err(MmioError::OutOfRange);
        }
        Ok(())
    }

    /// Read `width` (1, 2 or 4) bytes, little-endian.
    fn read(&self, op: &str, offset: u32, width: u32) -> Result<u32, MmioError> {
        self.check(offset, width)?;
        let mut buf = [0u8; 4];
        self.io
            .read_exact_at(&mut buf[..width as usize], self.base + u64::from(offset))
            .map_err(|e| {
                emit(
                    "mmio-io-error",
                    &[
                        ("attempted", Value::Str(op)),
                        ("offset", Value::Hex(offset.into())),
                        ("error", Value::Str(&e.to_string())),
                    ],
                );
                MmioError::Io(e)
            })?;
        Ok(u32::from_le_bytes(buf))
    }

    pub fn read8(&self, offset: u32) -> Result<u8, MmioError> {
        self.read("read8", offset, 1).map(|v| v as u8)
    }

    pub fn read16(&self, offset: u32) -> Result<u16, MmioError> {
        self.read("read16", offset, 2).map(|v| v as u16)
    }

    pub fn read32(&self, offset: u32) -> Result<u32, MmioError> {
        self.read("read32", offset, 4)
    }

    /// A write by the shim itself, to a register the shim owns (for
    /// example, NVMe controller configuration and submission queue
    /// doorbells). Not filtered by the write list, which describes what
    /// drivers may write. Never reachable from a driver.
    pub(crate) fn write32_shim(&self, offset: u32, value: u32) -> Result<(), MmioError> {
        self.check(offset, 4)?;
        self.io
            .write_all_at(&value.to_le_bytes(), self.base + u64::from(offset))
            .map_err(|e| {
                emit(
                    "mmio-io-error",
                    &[
                        ("attempted", Value::Str("shim write32")),
                        ("offset", Value::Hex(offset.into())),
                        ("value", Value::Hex(value.into())),
                        ("error", Value::Str(&e.to_string())),
                    ],
                );
                MmioError::Io(e)
            })
    }

    /// A driver's write of `width` (1, 2 or 4) bytes, filtered by the
    /// write list against every byte it touches.
    fn write(&self, op: &str, offset: u32, width: u32, value: u32) -> Result<(), MmioError> {
        self.check(offset, width)?;
        if let Err(refusal) = self.deny.check_write(offset, width) {
            self.violations.fetch_add(1, Ordering::SeqCst);
            let (register, reason, source) = match refusal {
                WriteRefusal::Denied(e) => (e.name, e.reason, e.source),
                WriteRefusal::NotAllowed => (
                    "unlisted",
                    "not inside any entry of the device's allow list",
                    "allow list",
                ),
            };
            emit(
                "mmio-denied",
                &[
                    ("attempted", Value::Str(op)),
                    ("offset", Value::Hex(offset.into())),
                    ("value", Value::Hex(value.into())),
                    ("register", Value::Str(register)),
                    ("reason", Value::Str(reason)),
                    ("source", Value::Str(source)),
                    ("model", Value::Str(self.deny.model)),
                    ("list_kind", Value::Str(self.deny.kind.as_str())),
                ],
            );
            return Err(MmioError::Denied(refusal));
        }
        self.io
            .write_all_at(
                &value.to_le_bytes()[..width as usize],
                self.base + u64::from(offset),
            )
            .map_err(|e| {
                emit(
                    "mmio-io-error",
                    &[
                        ("attempted", Value::Str(op)),
                        ("offset", Value::Hex(offset.into())),
                        ("value", Value::Hex(value.into())),
                        ("error", Value::Str(&e.to_string())),
                    ],
                );
                MmioError::Io(e)
            })
    }

    pub fn write8(&self, offset: u32, value: u8) -> Result<(), MmioError> {
        self.write("write8", offset, 1, value.into())
    }

    pub fn write16(&self, offset: u32, value: u16) -> Result<(), MmioError> {
        self.write("write16", offset, 2, value.into())
    }

    pub fn write32(&self, offset: u32, value: u32) -> Result<(), MmioError> {
        self.write("write32", offset, 4, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deny::{ListKind, WriteEntry};
    use std::sync::Mutex;

    struct Fake(Mutex<Vec<u8>>);

    impl RegionIo for Fake {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
            let mem = self.0.lock().unwrap();
            let o = offset as usize;
            buf.copy_from_slice(&mem[o..o + buf.len()]);
            Ok(())
        }
        fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            let mut mem = self.0.lock().unwrap();
            let o = offset as usize;
            mem[o..o + buf.len()].copy_from_slice(buf);
            Ok(())
        }
    }

    static LIST: WriteList = WriteList {
        vendor: 0xffff,
        device: 0xffff,
        model: "test",
        kind: ListKind::Deny,
        bar: 0,
        reviewed: true,
        commands: None,
        entries: &[WriteEntry {
            offset: 0x10,
            len: 4,
            name: "NVMCTL",
            reason: "test",
            source: "test",
        }],
    };

    /// A 0x40-byte window starting 0x100 bytes into a 0x200-byte fake region.
    fn window() -> Window<Fake> {
        Window::new(Fake(Mutex::new(vec![0; 0x200])), 0x100, 0x40, &LIST)
    }

    #[test]
    fn read_write_round_trip_at_region_base() {
        let w = window();
        w.write32(0x8, 0xdead_beef).unwrap();
        assert_eq!(w.read32(0x8).unwrap(), 0xdead_beef);
        assert_eq!(
            &w.io.0.lock().unwrap()[0x108..0x10c],
            &[0xef, 0xbe, 0xad, 0xde]
        );
    }

    #[test]
    fn bounds_and_alignment() {
        let w = window();
        assert!(matches!(w.read32(0x2), Err(MmioError::Misaligned)));
        assert!(matches!(w.write32(0x3, 0), Err(MmioError::Misaligned)));
        assert!(w.read32(0x3c).is_ok());
        assert!(matches!(w.read32(0x40), Err(MmioError::OutOfRange)));
        assert!(matches!(w.write32(0x40, 0), Err(MmioError::OutOfRange)));
        assert!(matches!(w.read32(u32::MAX - 3), Err(MmioError::OutOfRange)));
        assert_eq!(w.violations(), 0);
    }

    #[test]
    fn narrow_accesses_are_aligned_little_endian_and_in_range() {
        let w = window();
        w.write32(0x8, 0x4433_2211).unwrap();
        assert_eq!(w.read8(0x8).unwrap(), 0x11);
        assert_eq!(w.read8(0xb).unwrap(), 0x44);
        assert_eq!(w.read16(0xa).unwrap(), 0x4433);
        w.write8(0x9, 0xaa).unwrap();
        assert_eq!(w.read32(0x8).unwrap(), 0x4433_aa11, "only one byte written");
        w.write16(0xa, 0xbbcc).unwrap();
        assert_eq!(w.read32(0x8).unwrap(), 0xbbcc_aa11);
        assert!(matches!(w.read16(0x9), Err(MmioError::Misaligned)));
        assert!(matches!(w.write16(0x1, 0), Err(MmioError::Misaligned)));
        assert!(w.read8(0x3f).is_ok());
        assert!(matches!(w.read8(0x40), Err(MmioError::OutOfRange)));
        assert!(matches!(w.write16(0x40, 0), Err(MmioError::OutOfRange)));
        assert_eq!(w.violations(), 0);
    }

    #[test]
    fn narrow_writes_are_checked_byte_by_byte() {
        // NVMCTL is the dword at 0x10: every narrow write inside it is
        // denied, and the neighbours are not.
        let w = window();
        for (off, width) in [(0x10, 1), (0x13, 1), (0x12, 2)] {
            let r = if width == 1 {
                w.write8(off, 0xff)
            } else {
                w.write16(off, 0xffff)
            };
            assert!(matches!(r, Err(MmioError::Denied(_))), "0x{off:x}");
        }
        assert_eq!(w.violations(), 3);
        assert_eq!(&w.io.0.lock().unwrap()[0x110..0x114], &[0, 0, 0, 0]);
        assert!(w.write8(0x0f, 1).is_ok());
        assert!(w.write16(0x14, 1).is_ok());
    }

    #[test]
    fn denied_write_is_refused_counted_and_not_performed() {
        let w = window();
        let r = w.write32(0x10, 0x1234_5678);
        assert!(matches!(r, Err(MmioError::Denied(WriteRefusal::Denied(e))) if e.name == "NVMCTL"));
        assert_eq!(w.violations(), 1);
        assert_eq!(&w.io.0.lock().unwrap()[0x110..0x114], &[0, 0, 0, 0]);
        // Reads of a denied register are allowed.
        assert_eq!(w.read32(0x10).unwrap(), 0);
        assert_eq!(w.violations(), 1);
    }

    static ALLOW: WriteList = WriteList {
        vendor: 0xffff,
        device: 0xfffe,
        model: "test allow",
        kind: ListKind::Allow,
        bar: 0,
        reviewed: true,
        commands: None,
        entries: &[WriteEntry {
            offset: 0x8,
            len: 4,
            name: "CTRL",
            reason: "test",
            source: "test",
        }],
    };

    #[test]
    fn allow_list_window_refuses_unlisted_writes_and_permits_listed_ones() {
        let w = Window::new(Fake(Mutex::new(vec![0; 0x200])), 0x100, 0x40, &ALLOW);
        w.write32(0x8, 0xdead_beef).unwrap();
        assert_eq!(w.read32(0x8).unwrap(), 0xdead_beef);
        assert_eq!(w.violations(), 0);

        let r = w.write32(0xc, 0x1234_5678);
        assert!(matches!(
            r,
            Err(MmioError::Denied(WriteRefusal::NotAllowed))
        ));
        assert_eq!(w.violations(), 1);
        assert_eq!(&w.io.0.lock().unwrap()[0x10c..0x110], &[0, 0, 0, 0]);
        // Reads stay allowed everywhere.
        assert_eq!(w.read32(0xc).unwrap(), 0);
        assert_eq!(w.violations(), 1);
    }
}
