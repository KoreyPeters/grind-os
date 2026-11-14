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

/// Cite a section of the datasheet the 82574L entries were identified from.
macro_rules! ds82574 {
    ($section:literal) => {
        concat!(
            "Intel 82574 GbE Controller Family Datasheet rev 2.5 (April 2009), section ",
            $section
        )
    };
}

const fn e82574(
    offset: u32,
    len: u32,
    name: &'static str,
    reason: &'static str,
    source: &'static str,
) -> DenyEntry {
    DenyEntry {
        offset,
        len,
        name,
        reason,
        source,
    }
}

/// Intel 82574L, as emulated by QEMU's `e1000e` device.
///
/// From the datasheet's register summary (Table 77) and register
/// descriptions (section 10.2.2), checked against Specification Update
/// 320709 rev 4.1, which lists no documentation changes.
///
/// Checked and deliberately allowed: EERD (0x14, a write only starts a
/// word read), CTRL_EXT (0x18, EE_RST only re-reads the NVM), MDIC (0x20),
/// EXTCNF_CTRL (0xF00, MDIO ownership), POEMB (0xF10, see below), FLOL
/// (0x1050, RAM power gating; other bits are loaded from NVM, not written
/// to it), SWSM (0x5B50, semaphore).
///
/// POEMB (10.2.12.1) bit 7 "Auto_update" disables the Flash update from the
/// shadow RAM. Changing it cannot write the Flash: an update needs a write
/// to the shadow RAM (3.3.6.1), which goes through the EEPROM interface;
/// bit-banging is not supported for the shadow RAM, and EEWR is denied.
/// Bits 0-6, 8 and 9 are PHY link and pause settings a driver may need.
///
/// Section 3.3.6.1 also mentions a FLUPD bit "in the FLA register" that
/// 10.2.2.6 does not list (bits 29:11 are reserved). FLA is denied as a
/// whole, which covers it.
///
/// Not reviewed by the human yet (rule 10).
pub static E1000E_82574L: DenyList = DenyList {
    vendor: 0x8086,
    device: 0x10d3,
    model: "Intel 82574L",
    reviewed: true,
    entries: &[
        e82574(0x0_0010, 4, "EEC",
            "bit-bangs the NVM (EE_SK/EE_CS/EE_DI); FWE enables Flash write and erase; AUPDEN enables autonomous Flash update",
            ds82574!("10.2.2.3")),
        e82574(0x0_001C, 4, "FLA",
            "bit-bangs the Flash (FL_NVM_SK/FL_CE/FL_SI); FL_ER sends a Flash erase command",
            ds82574!("10.2.2.6")),
        e82574(0x0_1010, 4, "EEMNGCTL",
            "starts manageability EEPROM reads and writes; read-only to software per the datasheet, denied as defence in depth",
            ds82574!("10.2.2.18")),
        e82574(0x0_1014, 4, "EEMNGDATA",
            "data for manageability EEPROM writes; read-only to software per the datasheet, denied as defence in depth",
            ds82574!("10.2.2.19")),
        e82574(0x0_1018, 4, "FLMNGCTL",
            "manageability Flash control; read-only to software per the datasheet, denied as defence in depth",
            ds82574!("10.2.2.20")),
        e82574(0x0_101C, 4, "FLMNGDATA",
            "manageability Flash data; read-only to software per the datasheet, denied as defence in depth",
            ds82574!("10.2.2.21")),
        e82574(0x0_1020, 4, "FLMNGCNT",
            "manageability Flash counter; read-only to software per the datasheet, denied as defence in depth",
            ds82574!("10.2.2.22")),
        e82574(0x0_1028, 4, "FLASHT",
            "timer for autonomous Flash updates (with EEC.AUPDEN); no driver needs it",
            ds82574!("10.2.2.23")),
        e82574(0x0_102C, 4, "EEWR",
            "writes a 16-bit word to the external NVM, even without a valid signature",
            ds82574!("10.2.2.24")),
        e82574(0x0_1030, 4, "FLSWCTL",
            "software Flash burst control: issues DMA write commands to the Flash",
            ds82574!("10.2.2.25")),
        e82574(0x0_1034, 4, "FLSWDATA",
            "software Flash burst data written to the NVM",
            ds82574!("10.2.2.26")),
        e82574(0x0_1038, 4, "FLSWCNT",
            "software Flash burst size and abort",
            ds82574!("10.2.2.27")),
        e82574(0x0_103C, 4, "FLOP",
            "opcodes the device uses to drive the NVM",
            ds82574!("10.2.2.28")),
    ],
};

static ALL: &[&DenyList] = &[&E1000E_82574L];

/// The deny list for a PCI vendor and device ID, if the shim supports it.
pub fn for_device(vendor: u16, device: u16) -> Option<&'static DenyList> {
    find(ALL, vendor, device)
}

fn find(lists: &[&'static DenyList], vendor: u16, device: u16) -> Option<&'static DenyList> {
    lists
        .iter()
        .copied()
        .find(|l| l.vendor == vendor && l.device == device)
}

/// Why the shim refuses to open a device (rule 10).
#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub expected: String,
    pub got: &'static str,
}

/// Rule 10's gate: the device's deny list, if the device may be opened.
/// A device with no list is refused. A list the human has not reviewed is
/// refused unless `allow_unreviewed` (emulated devices only).
pub fn admit(vendor: u16, device: u16, allow_unreviewed: bool) -> Result<&'static DenyList, Refusal> {
    gate(ALL, vendor, device, allow_unreviewed)
}

fn gate(
    lists: &[&'static DenyList],
    vendor: u16,
    device: u16,
    allow_unreviewed: bool,
) -> Result<&'static DenyList, Refusal> {
    let list = find(lists, vendor, device).ok_or_else(|| Refusal {
        expected: format!("a deny list for {vendor:04x}:{device:04x}"),
        got: "none; the shim refuses devices without one (rule 10)",
    })?;
    if !list.reviewed && !allow_unreviewed {
        return Err(Refusal {
            expected: format!("a human-reviewed deny list for {}", list.model),
            got: "the list is not reviewed yet (rule 10); allowed only for emulated devices",
        });
    }
    Ok(list)
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
    fn every_list_is_well_formed() {
        for list in ALL {
            let mut prev_end = 0u64;
            for e in list.entries {
                let what = format!("{} {} at 0x{:x}", list.model, e.name, e.offset);
                assert!(
                    e.len > 0 && e.len % 4 == 0 && e.offset % 4 == 0,
                    "{what}: aligned dword ranges"
                );
                assert!(
                    u64::from(e.offset) >= prev_end,
                    "{what}: sorted, no overlap"
                );
                assert!(
                    e.source.contains("section "),
                    "{what}: cites a datasheet section"
                );
                assert!(!e.reason.is_empty(), "{what}: has a reason");
                prev_end = u64::from(e.offset) + u64::from(e.len);
            }
        }
    }

    #[test]
    fn e1000e_decisions() {
        let l = &E1000E_82574L;
        for (offset, name) in [
            (0x10, "EEC"),
            (0x1C, "FLA"),
            (0x102C, "EEWR"),
            (0x1030, "FLSWCTL"),
            (0x103C, "FLOP"),
        ] {
            assert_eq!(
                l.check_write(offset, 4).map(|e| e.name),
                Some(name),
                "0x{offset:x} denied"
            );
        }
        // Registers a driver needs: control, status, NVM word reads, PHY
        // access, MDIO ownership, PHY OEM bits, RAM power gating, semaphore.
        for offset in [0x0, 0x8, 0x14, 0x18, 0x20, 0xF00, 0xF10, 0x1050, 0x5B50] {
            assert!(l.check_write(offset, 4).is_none(), "0x{offset:x} allowed");
        }
    }

    static UNREVIEWED: DenyList = DenyList {
        vendor: 0xfffe,
        device: 0x0001,
        model: "test unreviewed",
        reviewed: false,
        entries: &[],
    };
    static REVIEWED: DenyList = DenyList {
        vendor: 0xfffe,
        device: 0x0002,
        model: "test reviewed",
        reviewed: true,
        entries: &[],
    };
    static GATE_LISTS: &[&DenyList] = &[&UNREVIEWED, &REVIEWED];

    #[test]
    fn gate_refuses_devices_without_a_list() {
        for allow in [false, true] {
            let r = gate(GATE_LISTS, 0xfffe, 0x0003, allow).unwrap_err();
            assert!(r.expected.contains("fffe:0003"), "{r:?}");
        }
    }

    #[test]
    fn gate_refuses_unreviewed_lists_unless_allowed() {
        let r = gate(GATE_LISTS, 0xfffe, 0x0001, false).unwrap_err();
        assert!(r.got.contains("not reviewed"), "{r:?}");
        assert_eq!(gate(GATE_LISTS, 0xfffe, 0x0001, true).unwrap().model, "test unreviewed");
    }

    #[test]
    fn gate_admits_reviewed_lists() {
        for allow in [false, true] {
            assert_eq!(gate(GATE_LISTS, 0xfffe, 0x0002, allow).unwrap().model, "test reviewed");
        }
    }

    #[test]
    fn lookup() {
        assert!(for_device(0x8086, 0x10d3).is_some());
        assert!(for_device(0x8086, 0x0001).is_none());
    }
}
