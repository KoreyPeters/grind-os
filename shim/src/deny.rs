// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-device register deny lists (rule 10).
//!
//! The IOMMU stops stray memory access but not deliberate register writes.
//! Each supported device has a list of register ranges that could
//! permanently damage hardware (anything that writes flash, NVM or
//! firmware), identified from the device's datasheet. The shim refuses and
//! logs every write that touches a listed range.
//!
//! A device with no list here cannot be opened at all. A list that has not
//! been reviewed by the human is refused unless the caller explicitly allows
//! it (for emulated devices only).

/// One denied register range in the register window.
#[derive(Debug)]
pub struct DenyEntry {
    /// Byte offset in the register window (BAR0).
    pub offset: u32,
    /// Length in bytes.
    pub len: u32,
    /// The register's name in the datasheet.
    pub name: &'static str,
    /// Why writing it could damage hardware.
    pub reason: &'static str,
    /// Datasheet document, revision and section it came from.
    pub source: &'static str,
}

/// The deny list for one device model.
#[derive(Debug)]
pub struct DenyList {
    pub vendor: u16,
    pub device: u16,
    pub model: &'static str,
    /// True once the human has reviewed `entries` against the datasheet.
    pub reviewed: bool,
    pub entries: &'static [DenyEntry],
}

impl DenyList {
    /// The first entry a write of `len` bytes at `offset` would touch.
    pub fn check_write(&self, offset: u32, len: u32) -> Option<&'static DenyEntry> {
        let start = u64::from(offset);
        let end = start + u64::from(len);
        self.entries.iter().find(|e| {
            let e_start = u64::from(e.offset);
            let e_end = e_start + u64::from(e.len);
            start < e_end && e_start < end
        })
    }
}

/// Intel 82574L, as emulated by QEMU's `e1000e` device.
///
/// PENDING: entries must come from the Intel 82574 GbE Controller Family
/// datasheet, which could not be read yet in this environment. Until then
/// the list is empty and unreviewed, so the host refuses it unless
/// unreviewed lists are explicitly allowed.
pub static E1000E_82574L: DenyList = DenyList {
    vendor: 0x8086,
    device: 0x10d3,
    model: "Intel 82574L",
    reviewed: false,
    entries: &[],
};

static ALL: &[&DenyList] = &[&E1000E_82574L];

/// The deny list for a PCI vendor and device ID, if the shim supports it.
pub fn for_device(vendor: u16, device: u16) -> Option<&'static DenyList> {
    ALL.iter()
        .copied()
        .find(|l| l.vendor == vendor && l.device == device)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LIST: DenyList = DenyList {
        vendor: 0xffff,
        device: 0xffff,
        model: "test",
        reviewed: true,
        entries: &[
            DenyEntry {
                offset: 0x10,
                len: 4,
                name: "A",
                reason: "test",
                source: "test",
            },
            DenyEntry {
                offset: 0x100,
                len: 0x10,
                name: "B",
                reason: "test",
                source: "test",
            },
        ],
    };

    #[test]
    fn overlap_rules() {
        assert!(TEST_LIST.check_write(0x0c, 4).is_none());
        assert_eq!(TEST_LIST.check_write(0x10, 4).unwrap().name, "A");
        assert!(TEST_LIST.check_write(0x14, 4).is_none());
        assert_eq!(TEST_LIST.check_write(0x0e, 4).unwrap().name, "A"); // partial overlap
        assert_eq!(TEST_LIST.check_write(0x10c, 4).unwrap().name, "B");
        assert!(TEST_LIST.check_write(0x110, 4).is_none());
        assert!(TEST_LIST.check_write(u32::MAX - 3, 4).is_none()); // no overflow
    }

    #[test]
    fn lookup() {
        assert!(for_device(0x8086, 0x10d3).is_some());
        assert!(for_device(0x8086, 0x0001).is_none());
    }
}
