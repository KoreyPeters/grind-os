// SPDX-License-Identifier: AGPL-3.0-or-later

//! Per-device register write lists (rule 10).
//!
//! The IOMMU stops stray memory access but not deliberate register writes.
//! Each supported device has a list that decides which register writes the
//! shim refuses, so no driver can permanently damage hardware (anything
//! that writes flash, NVM or firmware). A list is one of two kinds:
//!
//! - **Deny** (from the datasheet): writes that touch a listed range are
//!   refused; all others pass.
//! - **Allow** (from a reference driver's source or traces): only writes
//!   that fall entirely inside a listed range pass; all others are refused.
//!
//! Either way, a refused write is logged and counted, and fails the run.
//!
//! A device that fetches commands from memory (NVMe) also has a command
//! allow list ([`crate::nvme::CommandList`]), attached to its write list.
//! The device counts as reviewed only when both lists are.
//!
//! A device with no list here cannot be opened at all. A list that has not
//! been reviewed by the human is refused unless the caller explicitly allows
//! it (for emulated devices only).

/// Whether a list names the writes to refuse or the only writes to permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// From the datasheet: writes touching an entry are refused.
    Deny,
    /// From a reference driver: writes outside every entry are refused.
    Allow,
}

impl ListKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ListKind::Deny => "deny",
            ListKind::Allow => "allow",
        }
    }
}

/// One register range in a write list.
#[derive(Debug)]
pub struct WriteEntry {
    /// Byte offset in the register window (BAR0).
    pub offset: u32,
    /// Length in bytes.
    pub len: u32,
    /// The register's name in the source.
    pub name: &'static str,
    /// Deny list: why writing it could damage hardware. Allow list: what
    /// the reference driver writes it for.
    pub reason: &'static str,
    /// Where the entry came from: a datasheet document, revision and
    /// section; or a file at an exact commit; or a trace (rule 7).
    pub source: &'static str,
}

/// The write list for one device model.
#[derive(Debug)]
pub struct WriteList {
    pub vendor: u16,
    pub device: u16,
    pub model: &'static str,
    pub kind: ListKind,
    /// True once the human has reviewed `entries` against their sources.
    pub reviewed: bool,
    /// For a device that fetches commands from memory: the commands the
    /// shim lets a driver submit (rule 10).
    pub commands: Option<&'static crate::nvme::CommandList>,
    pub entries: &'static [WriteEntry],
}

/// Why a write was refused.
#[derive(Debug, Clone, Copy)]
pub enum WriteRefusal {
    /// Deny list: the write touches this entry.
    Denied(&'static WriteEntry),
    /// Allow list: the write is not entirely inside any entry.
    NotAllowed,
}

impl WriteList {
    /// Whether the human has reviewed this list and, if the device takes
    /// commands, its command list.
    pub fn fully_reviewed(&self) -> bool {
        self.reviewed && self.commands.is_none_or(|c| c.reviewed)
    }

    /// Whether a write of `len` bytes at `offset` may go to the device.
    pub fn check_write(&self, offset: u32, len: u32) -> Result<(), WriteRefusal> {
        let start = u64::from(offset);
        let end = start + u64::from(len);
        let range = |e: &WriteEntry| (u64::from(e.offset), u64::from(e.offset) + u64::from(e.len));
        match self.kind {
            ListKind::Deny => match self.entries.iter().find(|e| {
                let (e_start, e_end) = range(e);
                start < e_end && e_start < end
            }) {
                Some(e) => Err(WriteRefusal::Denied(e)),
                None => Ok(()),
            },
            ListKind::Allow => {
                if self.entries.iter().any(|e| {
                    let (e_start, e_end) = range(e);
                    e_start <= start && end <= e_end
                }) {
                    Ok(())
                } else {
                    Err(WriteRefusal::NotAllowed)
                }
            }
        }
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
) -> WriteEntry {
    WriteEntry {
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
/// Reviewed by the human against the datasheet (rule 10).
pub static E1000E_82574L: WriteList = WriteList {
    vendor: 0x8086,
    device: 0x10d3,
    model: "Intel 82574L",
    kind: ListKind::Deny,
    reviewed: true,
    commands: None,
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

/// The FreeBSD NVMe source the QEMU NVMe list cites.
#[allow(unused_macros)]
macro_rules! freebsd_nvme {
    ($file_line:literal) => {
        concat!(
            "FreeBSD sys/dev/nvme/",
            $file_line,
            " at commit 74d72b3491e810068313e49629c3b8fe46f29318 (BSD-2-Clause)"
        )
    };
}

/// I/O queue IDs a driver may create: 1 to 64 (QEMU's default
/// `max_ioqpairs`).
pub const NVME_MAX_IO_QUEUES: u16 = 64;

/// QEMU's emulated NVMe controller (`-device nvme`).
///
/// An allow list from a reference driver (rule 10's third kind), taken from
/// FreeBSD's nvme(4) source. In normal operation FreeBSD writes only CC,
/// ASQ, ACQ, AQA, INTMS, INTMC, the submission-queue tail doorbells and the
/// completion-queue head doorbells (nvme_ctrlr.c:346, 387, 388, 396, 424,
/// 1537, 1540, 2089; nvme_qpair.c:480, 1085). The shim owns CC, ASQ, ACQ,
/// AQA and every queue with its doorbells (rule 10: the queues the device
/// fetches from, and the registers that control or ring them; and the
/// completion queues, so that the shim learns from the device how far each
/// submission queue has been fetched). That leaves INTMS and INTMC
/// (nvme_ctrlr.c:1537, 1540), the interrupt masks. Drivers poll and never
/// need them, so the human chose to leave them out too (2026-11-14): the
/// list is empty and every driver register write is refused. FreeBSD never
/// writes NSSR, the CMB, PMR or boot-partition registers (nvme.h:817-853).
///
/// The source macro stays for when an entry is added.
///
/// Not yet reviewed by the human.
pub static QEMU_NVME: WriteList = WriteList {
    vendor: 0x1b36,
    device: 0x0010,
    model: "QEMU NVM Express controller",
    kind: ListKind::Allow,
    reviewed: false,
    commands: Some(&crate::nvme::NVME_COMMANDS),
    entries: &[],
};

static ALL: &[&WriteList] = &[&E1000E_82574L, &QEMU_NVME];

/// The write list for a PCI vendor and device ID, if the shim supports it.
pub fn for_device(vendor: u16, device: u16) -> Option<&'static WriteList> {
    find(ALL, vendor, device)
}

fn find(lists: &[&'static WriteList], vendor: u16, device: u16) -> Option<&'static WriteList> {
    lists
        .iter()
        .copied()
        .find(|l| l.vendor == vendor && l.device == device)
}

/// Why the shim refuses to open a device (rule 10).
#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub expected: String,
    pub got: String,
}

/// Where the machine's DMI system vendor is read from.
pub const DMI_SYS_VENDOR: &str = "/sys/class/dmi/id/sys_vendor";

/// DMI system vendors that identify an emulator. Only under one of these
/// is an unreviewed list honoured. QEMU's q35 machine reports "QEMU".
const EMULATOR_SYS_VENDORS: &[&str] = &["QEMU"];

/// The machine's DMI system vendor, trimmed, if it can be read.
pub fn sys_vendor() -> Option<String> {
    std::fs::read_to_string(DMI_SYS_VENDOR)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Rule 10's gate: the device's write list, if the device may be opened.
///
/// - A device with no list is refused.
/// - A reviewed list is admitted.
/// - An unreviewed list is refused unless `allow_unreviewed` is set *and*
///   the DMI system vendor (`sys_vendor`) names an emulator. The flag alone
///   is not enough: on real hardware it is refused.
///
/// Limit: DMI identifies the machine, not the device. A real device passed
/// through to a QEMU guest would pass this check.
pub fn admit(
    vendor: u16,
    device: u16,
    allow_unreviewed: bool,
    sys_vendor: Option<&str>,
) -> Result<&'static WriteList, Refusal> {
    gate(ALL, vendor, device, allow_unreviewed, sys_vendor)
}

fn gate(
    lists: &[&'static WriteList],
    vendor: u16,
    device: u16,
    allow_unreviewed: bool,
    sys_vendor: Option<&str>,
) -> Result<&'static WriteList, Refusal> {
    let list = find(lists, vendor, device).ok_or_else(|| Refusal {
        expected: format!("a write list (rule 10) for {vendor:04x}:{device:04x}"),
        got: "none; the shim refuses devices without one (rule 10)".into(),
    })?;
    if list.fully_reviewed() {
        return Ok(list);
    }
    if !allow_unreviewed {
        let commands = if list.commands.is_some() {
            " and command list"
        } else {
            ""
        };
        return Err(Refusal {
            expected: format!(
                "a human-reviewed {} list{commands} for {}",
                list.kind.as_str(),
                list.model
            ),
            got: "the list is not reviewed yet (rule 10); allowed only for emulated devices".into(),
        });
    }
    match sys_vendor.map(str::trim) {
        Some(v) if EMULATOR_SYS_VENDORS.contains(&v) => Ok(list),
        other => Err(Refusal {
            expected: format!(
                "an emulator (DMI sys_vendor one of {}) before honouring \
                 --allow-unreviewed-deny-list for {}",
                EMULATOR_SYS_VENDORS.join(", "),
                list.model
            ),
            got: match other {
                Some(v) => format!("DMI sys_vendor {v:?}: not an emulator; the unreviewed list is refused (rule 10)"),
                None => format!("{DMI_SYS_VENDOR} unreadable; the unreviewed list is refused (rule 10)"),
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LIST: WriteList = WriteList {
        vendor: 0xffff,
        device: 0xffff,
        model: "test",
        kind: ListKind::Deny,
        reviewed: true,
        commands: None,
        entries: &[
            WriteEntry {
                offset: 0x10,
                len: 4,
                name: "A",
                reason: "test",
                source: "test",
            },
            WriteEntry {
                offset: 0x100,
                len: 0x10,
                name: "B",
                reason: "test",
                source: "test",
            },
        ],
    };

    /// The entry a deny-list refusal names, or None if the write passes.
    fn denied(list: &WriteList, offset: u32, len: u32) -> Option<&'static str> {
        match list.check_write(offset, len) {
            Ok(()) => None,
            Err(WriteRefusal::Denied(e)) => Some(e.name),
            Err(WriteRefusal::NotAllowed) => panic!("a deny list never answers NotAllowed"),
        }
    }

    #[test]
    fn overlap_rules() {
        assert_eq!(denied(&TEST_LIST, 0x0c, 4), None);
        assert_eq!(denied(&TEST_LIST, 0x10, 4), Some("A"));
        assert_eq!(denied(&TEST_LIST, 0x14, 4), None);
        assert_eq!(denied(&TEST_LIST, 0x0e, 4), Some("A")); // partial overlap
        assert_eq!(denied(&TEST_LIST, 0x10c, 4), Some("B"));
        assert_eq!(denied(&TEST_LIST, 0x110, 4), None);
        assert_eq!(denied(&TEST_LIST, u32::MAX - 3, 4), None); // no overflow
    }

    static ALLOW_LIST: WriteList = WriteList {
        vendor: 0xffff,
        device: 0xfffe,
        model: "test allow",
        kind: ListKind::Allow,
        reviewed: true,
        commands: None,
        entries: &[
            WriteEntry {
                offset: 0x0,
                len: 4,
                name: "CTRL",
                reason: "test",
                source: "test",
            },
            WriteEntry {
                offset: 0x100,
                len: 0x10,
                name: "RING",
                reason: "test",
                source: "test",
            },
        ],
    };

    #[test]
    fn allow_list_passes_only_writes_inside_an_entry() {
        let l = &ALLOW_LIST;
        // Inside, including both edges of a multi-dword entry.
        for (offset, len) in [(0x0, 4), (0x100, 4), (0x10c, 4), (0x100, 0x10)] {
            assert!(
                l.check_write(offset, len).is_ok(),
                "0x{offset:x}+{len} allowed"
            );
        }
        // Outside: unlisted gaps, just before, just after, straddling an
        // edge, and the top of the address space.
        for (offset, len) in [
            (0x4, 4),
            (0xfc, 4),
            (0x110, 4),
            (0xfe, 4),
            (0x10e, 4),
            (0x10c, 8),
            (0x0, 8),
            (u32::MAX - 3, 4),
        ] {
            assert!(
                matches!(l.check_write(offset, len), Err(WriteRefusal::NotAllowed)),
                "0x{offset:x}+{len} refused"
            );
        }
    }

    #[test]
    fn empty_allow_list_refuses_everything() {
        static EMPTY: WriteList = WriteList {
            vendor: 0xffff,
            device: 0xfffd,
            model: "test empty allow",
            kind: ListKind::Allow,
            reviewed: true,
            commands: None,
            entries: &[],
        };
        for offset in [0x0, 0x8, 0x100, 0x1fffc] {
            assert!(matches!(
                EMPTY.check_write(offset, 4),
                Err(WriteRefusal::NotAllowed)
            ));
        }
    }

    /// The rules every shipped list must meet; the first broken one, if any.
    fn well_formed(list: &WriteList) -> Result<(), String> {
        let mut prev_end = 0u64;
        for e in list.entries {
            let what = format!("{} {} at 0x{:x}", list.model, e.name, e.offset);
            if !(e.len > 0 && e.len % 4 == 0 && e.offset % 4 == 0) {
                return Err(format!("{what}: not an aligned dword range"));
            }
            if u64::from(e.offset) < prev_end {
                return Err(format!(
                    "{what}: not sorted, or overlaps the previous entry"
                ));
            }
            // Rule 7: deny lists cite a datasheet section; allow lists cite
            // a file at an exact commit, or a trace.
            let cited = match list.kind {
                ListKind::Deny => e.source.contains("section "),
                ListKind::Allow => e.source.contains(" commit ") || e.source.starts_with("trace "),
            };
            if !cited {
                return Err(format!(
                    "{what}: {} list entry does not cite its source: {:?}",
                    list.kind.as_str(),
                    e.source
                ));
            }
            if e.reason.is_empty() {
                return Err(format!("{what}: no reason"));
            }
            prev_end = u64::from(e.offset) + u64::from(e.len);
        }
        Ok(())
    }

    #[test]
    fn every_list_is_well_formed() {
        for list in ALL {
            if let Err(e) = well_formed(list) {
                panic!("{e}");
            }
        }
    }

    #[test]
    fn well_formedness_checks_allow_list_citations() {
        static FILE: [WriteEntry; 1] = [WriteEntry {
            offset: 0x0,
            len: 4,
            name: "CTRL",
            reason: "reset and link setup",
            source: "FreeBSD sys/dev/e1000/e1000_ich8lan.c at commit 0123abcd",
        }];
        static TRACE: [WriteEntry; 1] = [WriteEntry {
            offset: 0x0,
            len: 4,
            name: "CTRL",
            reason: "reset and link setup",
            source: "trace optiplex-7050-i219lm-2026-11-20.json",
        }];
        static SECTION: [WriteEntry; 1] = [WriteEntry {
            offset: 0x0,
            len: 4,
            name: "CTRL",
            reason: "reset and link setup",
            source: "Some datasheet, section 10.2.2.1",
        }];
        let list = |entries: &'static [WriteEntry]| WriteList {
            vendor: 0,
            device: 0,
            model: "cite",
            kind: ListKind::Allow,
            reviewed: false,
            commands: None,
            entries,
        };
        assert!(well_formed(&list(&FILE)).is_ok());
        assert!(well_formed(&list(&TRACE)).is_ok());
        let err = well_formed(&list(&SECTION)).unwrap_err();
        assert!(err.contains("does not cite"), "{err}");
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
            assert_eq!(denied(l, offset, 4), Some(name), "0x{offset:x} denied");
        }
        // Registers a driver needs: control, status, NVM word reads, PHY
        // access, MDIO ownership, PHY OEM bits, RAM power gating, semaphore.
        for offset in [0x0, 0x8, 0x14, 0x18, 0x20, 0xF00, 0xF10, 0x1050, 0x5B50] {
            assert_eq!(denied(l, offset, 4), None, "0x{offset:x} allowed");
        }
    }

    static UNREVIEWED: WriteList = WriteList {
        vendor: 0xfffe,
        device: 0x0001,
        model: "test unreviewed",
        kind: ListKind::Deny,
        reviewed: false,
        commands: None,
        entries: &[],
    };
    static REVIEWED: WriteList = WriteList {
        vendor: 0xfffe,
        device: 0x0002,
        model: "test reviewed",
        kind: ListKind::Deny,
        reviewed: true,
        commands: None,
        entries: &[],
    };
    static GATE_LISTS: &[&WriteList] = &[&UNREVIEWED, &REVIEWED];

    const QEMU: Option<&str> = Some("QEMU");
    const DELL: Option<&str> = Some("Dell Inc.");

    static UNREVIEWED_COMMANDS: crate::nvme::CommandList = crate::nvme::CommandList {
        model: "test commands",
        reviewed: false,
        entries: &[],
    };
    static REVIEWED_REGS_ONLY: WriteList = WriteList {
        vendor: 0xfffe,
        device: 0x0003,
        model: "test commands unreviewed",
        kind: ListKind::Allow,
        reviewed: true,
        commands: Some(&UNREVIEWED_COMMANDS),
        entries: &[],
    };

    #[test]
    fn gate_needs_the_command_list_reviewed_too() {
        let lists: &[&WriteList] = &[&REVIEWED_REGS_ONLY];
        for sv in [QEMU, DELL, None] {
            let r = gate(lists, 0xfffe, 0x0003, false, sv).unwrap_err();
            assert!(r.expected.contains("and command list"), "{r:?}");
        }
        assert!(gate(lists, 0xfffe, 0x0003, true, QEMU).is_ok());
        assert!(gate(lists, 0xfffe, 0x0003, true, DELL).is_err());
    }

    #[test]
    fn gate_refuses_devices_without_a_list() {
        for allow in [false, true] {
            for sv in [QEMU, DELL, None] {
                let r = gate(GATE_LISTS, 0xfffe, 0x0003, allow, sv).unwrap_err();
                assert!(r.expected.contains("fffe:0003"), "{r:?}");
            }
        }
    }

    #[test]
    fn gate_refuses_unreviewed_lists_without_the_flag() {
        for sv in [QEMU, DELL, None] {
            let r = gate(GATE_LISTS, 0xfffe, 0x0001, false, sv).unwrap_err();
            assert!(r.got.contains("not reviewed"), "{r:?}");
        }
    }

    #[test]
    fn gate_honours_the_flag_only_under_an_emulator() {
        assert_eq!(
            gate(GATE_LISTS, 0xfffe, 0x0001, true, QEMU).unwrap().model,
            "test unreviewed"
        );
        // sysfs values end in a newline.
        assert!(gate(GATE_LISTS, 0xfffe, 0x0001, true, Some("QEMU\n")).is_ok());

        let r = gate(GATE_LISTS, 0xfffe, 0x0001, true, DELL).unwrap_err();
        assert!(
            r.got.contains("\"Dell Inc.\"") && r.got.contains("not an emulator"),
            "{r:?}"
        );
        let r = gate(GATE_LISTS, 0xfffe, 0x0001, true, None).unwrap_err();
        assert!(r.got.contains("unreadable"), "{r:?}");
        // Exact match only: no prefixes, no case folding.
        for sv in ["qemu", "QEMU-ish", "", "Standard PC (Q35 + ICH9, 2009)"] {
            assert!(
                gate(GATE_LISTS, 0xfffe, 0x0001, true, Some(sv)).is_err(),
                "{sv:?}"
            );
        }
    }

    #[test]
    fn gate_admits_reviewed_lists_anywhere() {
        for allow in [false, true] {
            for sv in [QEMU, DELL, None] {
                assert_eq!(
                    gate(GATE_LISTS, 0xfffe, 0x0002, allow, sv).unwrap().model,
                    "test reviewed"
                );
            }
        }
    }

    #[test]
    fn lookup() {
        assert!(for_device(0x8086, 0x10d3).is_some());
        assert!(for_device(0x8086, 0x0001).is_none());
    }
}
