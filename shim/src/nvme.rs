// SPDX-License-Identifier: AGPL-3.0-or-later

//! NVMe: the shim's side of rule 10 for a device that fetches commands
//! from memory.
//!
//! The shim owns the controller's configuration (CC, AQA, ASQ, ACQ) and
//! every queue, in its own memory: the admin queue and all I/O submission
//! and completion queues, with their doorbells. A driver hands it 64-byte
//! commands. The shim checks each one against [`NVME_COMMANDS`] and the
//! driver's own DMA buffers, copies it into its own queue, and rings the
//! doorbell, so a driver can neither issue an unlisted command nor change a
//! command after the check.
//!
//! The shim reads every completion itself and hands copies to the driver.
//! That is how it learns, from the device and not from the driver, how far
//! the device has fetched each submission queue (SQHD), and it never
//! overwrites an entry the device may still be reading. If it did, the
//! device could read half of one checked command and half of another: a
//! mix that was never checked, whose addresses could reach the shim's own
//! queues.
//!
//! Every NVMe fact here comes from FreeBSD's nvme(4) or SPDK's
//! `nvme_spec.h` at the commits cited in [`NVME_COMMANDS`] and
//! docs/design/milestone-5-nvme.md. The NVMe specification is not a source.
//!
//! The command checks ([`check`]) are a pure function, unit-tested below.
//! The rest drives the device and is exercised in the test VM.

use crate::dma::{DmaAllocator, DmaBuffer};
use crate::log::{emit, Value};
use crate::window::{MmioError, RegionIo, Window};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cite FreeBSD's nvme(4) at the pinned commit.
macro_rules! freebsd {
    ($file_line:literal) => {
        concat!(
            "FreeBSD sys/dev/nvme/",
            $file_line,
            " at commit 74d72b3491e810068313e49629c3b8fe46f29318 (BSD-2-Clause)"
        )
    };
}

/// Cite SPDK's nvme_spec.h at the pinned commit.
macro_rules! spdk {
    ($line:literal) => {
        concat!(
            "SPDK include/spdk/nvme_spec.h:",
            $line,
            " at commit 91404c6437e45395a943bfeaf48f58ea1d6aff74 (BSD-3-Clause)"
        )
    };
}

// ---------------------------------------------------------------------
// The command allow list.

/// Which kind of queue a command is submitted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    Admin,
    Io,
}

impl QueueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueKind::Admin => "admin",
            QueueKind::Io => "io",
        }
    }
}

/// The conditions a listed command must meet. Each is implemented in
/// [`check`]; every field not named there must be zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Identify: the controller (CNS 1) or a namespace (CNS 0) only, the
    /// two FreeBSD uses; a 4096-byte data buffer inside the driver's
    /// buffers.
    Identify,
    /// Get Features: Number of Queues only, no data buffer.
    GetNumberOfQueues,
    /// Set Features: Number of Queues only, Save clear, no data buffer.
    SetNumberOfQueues,
    /// Create I/O Completion Queue: PRP1 = 0; the shim allocates the
    /// queue. Physically contiguous, interrupts disabled.
    CreateIoCq,
    /// Create I/O Submission Queue: PRP1 = 0; the shim allocates the queue.
    CreateIoSq,
    /// Delete I/O Completion Queue.
    DeleteIoCq,
    /// Delete I/O Submission Queue: the shim frees the queue afterwards.
    DeleteIoSq,
    /// Flush: no data, no options.
    Flush,
    /// Read or Write: no options; the data, (NLB + 1) blocks, inside the
    /// driver's buffers, at most two PRP entries (no PRP lists).
    ReadWrite,
}

/// One command a driver may submit.
#[derive(Debug)]
pub struct CommandEntry {
    pub queue: QueueKind,
    pub opcode: u8,
    pub name: &'static str,
    pub rule: Rule,
    /// Where the opcode and the fields its rule checks come from (rule 7).
    pub source: &'static str,
}

/// The commands the shim lets a driver submit to one device model. Every
/// other command is refused (rule 10).
#[derive(Debug)]
pub struct CommandList {
    pub model: &'static str,
    /// True once the human has reviewed `entries` against their sources.
    pub reviewed: bool,
    pub entries: &'static [CommandEntry],
}

impl CommandList {
    pub fn find(&self, queue: QueueKind, opcode: u8) -> Option<&'static CommandEntry> {
        self.entries
            .iter()
            .find(|e| e.queue == queue && e.opcode == opcode)
    }
}

/// NVMe commands for Phase 0, from FreeBSD's nvme(4) and SPDK.
///
/// Refused by omission, among others: Firmware Image Download (11h),
/// Firmware Commit (10h), Namespace Management (0Dh), Format NVM (80h),
/// Security Send (81h), Sanitize (84h) (FreeBSD nvme.h:1080-1124), every
/// vendor-specific admin opcode from C0h (SPDK nvme_spec.h:1803), and every
/// I/O opcode but Flush, Write and Read (FreeBSD nvme.h:1128-1147).
///
/// Reviewed by the human (rule 10).
pub static NVME_COMMANDS: CommandList = CommandList {
    model: "NVM Express (FreeBSD nvme(4) usage)",
    reviewed: true,
    entries: &[
        CommandEntry {
            queue: QueueKind::Admin,
            opcode: 0x00,
            name: "Delete I/O Submission Queue",
            rule: Rule::DeleteIoSq,
            source: freebsd!("nvme.h:1081; nvme_ctrlr_cmd.c:125-140 (QID in CDW10)"),
        },
        CommandEntry {
            queue: QueueKind::Admin,
            opcode: 0x01,
            name: "Create I/O Submission Queue",
            rule: Rule::CreateIoSq,
            source: freebsd!("nvme.h:1082, 257-259; nvme_ctrlr_cmd.c:109-119"),
        },
        CommandEntry {
            queue: QueueKind::Admin,
            opcode: 0x04,
            name: "Delete I/O Completion Queue",
            rule: Rule::DeleteIoCq,
            source: freebsd!("nvme.h:1085; nvme_ctrlr_cmd.c:125-140 (QID in CDW10)"),
        },
        CommandEntry {
            queue: QueueKind::Admin,
            opcode: 0x05,
            name: "Create I/O Completion Queue",
            rule: Rule::CreateIoCq,
            source: freebsd!("nvme.h:1086, 262-263; nvme_ctrlr_cmd.c:85-94"),
        },
        CommandEntry {
            queue: QueueKind::Admin,
            opcode: 0x06,
            name: "Identify",
            rule: Rule::Identify,
            source: freebsd!("nvme.h:1087; nvme_ctrlr_cmd.c:33-70 (CNS 1 at :49, CNS 0 at :65; 4096-byte payload)"),
        },
        CommandEntry {
            queue: QueueKind::Admin,
            opcode: 0x09,
            name: "Set Features (Number of Queues)",
            rule: Rule::SetNumberOfQueues,
            source: freebsd!(
                "nvme.h:1090, 782-785 (SV bit 31), 1158 (FID 07h); nvme_ctrlr_cmd.c:203-211"
            ),
        },
        CommandEntry {
            queue: QueueKind::Admin,
            opcode: 0x0A,
            name: "Get Features (Number of Queues)",
            rule: Rule::GetNumberOfQueues,
            source: freebsd!("nvme.h:1091, 776-779 (FID, SEL), 1158 (FID 07h)"),
        },
        CommandEntry {
            queue: QueueKind::Io,
            opcode: 0x00,
            name: "Flush",
            rule: Rule::Flush,
            source: freebsd!("nvme.h:1129, 2225"),
        },
        CommandEntry {
            queue: QueueKind::Io,
            opcode: 0x01,
            name: "Write",
            rule: Rule::ReadWrite,
            source: concat!(
                freebsd!("nvme.h:1130, 2230-2244"),
                "; NLB in CDW12 bits 15:0: ",
                spdk!("1386-1391")
            ),
        },
        CommandEntry {
            queue: QueueKind::Io,
            opcode: 0x02,
            name: "Read",
            rule: Rule::ReadWrite,
            source: concat!(
                freebsd!("nvme.h:1131, 2230-2251"),
                "; NLB in CDW12 bits 15:0: ",
                spdk!("1386-1391")
            ),
        },
    ],
};

// ---------------------------------------------------------------------
// Command layout and the pure checker.

/// Size of a submission queue entry (`struct nvme_command`, FreeBSD
/// nvme.h:890-921).
pub const COMMAND_BYTES: usize = 64;
/// Size of a completion queue entry (`struct nvme_completion`, nvme.h:926-940).
pub const COMPLETION_BYTES: usize = 16;
/// Memory page size the shim configures (CC.MPS = 0).
const PAGE: u64 = 4096;
/// Largest I/O submission queue the shim allocates: one page of entries.
pub const MAX_IO_SQ_ENTRIES: u32 = (PAGE as u32) / COMMAND_BYTES as u32;
/// Largest I/O completion queue the shim allocates: one page of entries.
pub const MAX_IO_CQ_ENTRIES: u32 = (PAGE as u32) / COMPLETION_BYTES as u32;
/// Feature ID for Number of Queues (FreeBSD nvme.h:1158).
const FID_NUMBER_OF_QUEUES: u32 = 0x07;

/// A 64-byte submission queue entry, decoded (FreeBSD nvme.h:890-921).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub opcode: u8,
    /// FUSE (bits 1:0) and PSDT (bits 7:6) (nvme.h:240-253).
    pub flags: u8,
    pub cid: u16,
    pub nsid: u32,
    pub cdw2: u32,
    pub cdw3: u32,
    pub mptr: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from(le32(b, at)) | (u64::from(le32(b, at + 4)) << 32)
}

impl Command {
    pub fn parse(b: &[u8; COMMAND_BYTES]) -> Command {
        Command {
            opcode: b[0],
            flags: b[1],
            cid: u16::from_le_bytes([b[2], b[3]]),
            nsid: le32(b, 4),
            cdw2: le32(b, 8),
            cdw3: le32(b, 12),
            mptr: le64(b, 16),
            prp1: le64(b, 24),
            prp2: le64(b, 32),
            cdw10: le32(b, 40),
            cdw11: le32(b, 44),
            cdw12: le32(b, 48),
            cdw13: le32(b, 52),
            cdw14: le32(b, 56),
            cdw15: le32(b, 60),
        }
    }

    pub fn encode(&self) -> [u8; COMMAND_BYTES] {
        let mut b = [0u8; COMMAND_BYTES];
        b[0] = self.opcode;
        b[1] = self.flags;
        b[2..4].copy_from_slice(&self.cid.to_le_bytes());
        b[4..8].copy_from_slice(&self.nsid.to_le_bytes());
        b[8..12].copy_from_slice(&self.cdw2.to_le_bytes());
        b[12..16].copy_from_slice(&self.cdw3.to_le_bytes());
        b[16..24].copy_from_slice(&self.mptr.to_le_bytes());
        b[24..32].copy_from_slice(&self.prp1.to_le_bytes());
        b[32..40].copy_from_slice(&self.prp2.to_le_bytes());
        for (i, d) in [
            self.cdw10, self.cdw11, self.cdw12, self.cdw13, self.cdw14, self.cdw15,
        ]
        .iter()
        .enumerate()
        {
            b[40 + 4 * i..44 + 4 * i].copy_from_slice(&d.to_le_bytes());
        }
        b
    }
}

/// Why the shim refused a command: which check, what it expected, what it
/// got. Logged as `command-refused`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub check: &'static str,
    pub expected: String,
    pub got: String,
}

fn refuse(check: &'static str, expected: impl Into<String>, got: impl Into<String>) -> Refusal {
    Refusal {
        check,
        expected: expected.into(),
        got: got.into(),
    }
}

/// What the device glue must do with a command that passed [`check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    /// Submit as is.
    Plain,
    /// Allocate a submission queue of `entries` entries for `qid`, bound to
    /// completion queue `cqid`, and put its address in PRP1.
    CreateSq { qid: u16, entries: u32, cqid: u16 },
    /// Allocate a completion queue of `entries` entries for `qid` and put
    /// its address in PRP1.
    CreateCq { qid: u16, entries: u32 },
    /// Free submission queue `qid` once the command succeeds.
    DeleteSq { qid: u16 },
    /// Forget completion queue `qid` once the command succeeds.
    DeleteCq { qid: u16 },
}

/// What [`check`] needs to know beyond the command itself.
pub struct Context<'a> {
    /// Whether `[iova, iova + len)` lies inside one of the driver's DMA
    /// buffers.
    pub driver_owns: &'a dyn Fn(u64, u64) -> bool,
    /// For Read and Write: the namespace's logical block size in bytes, or
    /// why the shim cannot use the namespace.
    pub block_bytes: Option<Result<u32, String>>,
    /// I/O queue IDs a driver may use: 1 to this.
    pub max_qid: u16,
}

fn zero(check: &'static str, what: &str, v: u64) -> Result<(), Refusal> {
    if v == 0 {
        Ok(())
    } else {
        Err(refuse(check, format!("{what} = 0"), format!("0x{v:x}")))
    }
}

/// Check the data pointer of a command that transfers `len` bytes, using
/// at most two PRP entries (FreeBSD nvme_qpair.c:573-575, 1109-1117):
/// PRP1 covers up to the end of its page, PRP2 the next page. Longer
/// transfers need a PRP list, which lives in driver memory the driver could
/// change after the check, so they are refused (rule 10).
fn check_prps(c: &Command, len: u64, ctx: &Context) -> Result<(), Refusal> {
    if len == 0 {
        zero("prp", "PRP1", c.prp1)?;
        return zero("prp", "PRP2", c.prp2);
    }
    // FreeBSD nvme_qpair.c:552: "NVMe PRP format is restricted to 4-byte
    // alignment."
    if !c.prp1.is_multiple_of(4) {
        return Err(refuse(
            "prp",
            "PRP1 4-byte aligned",
            format!("0x{:x}", c.prp1),
        ));
    }
    let first = len.min(PAGE - c.prp1 % PAGE);
    if !(ctx.driver_owns)(c.prp1, first) {
        return Err(refuse(
            "address",
            format!(
                "PRP1 range 0x{:x}+0x{first:x} inside one of the driver's DMA buffers",
                c.prp1
            ),
            "outside the driver's buffers",
        ));
    }
    let rest = len - first;
    if rest == 0 {
        return zero("prp", "PRP2 (the transfer fits in PRP1's page)", c.prp2);
    }
    if rest > PAGE {
        return Err(refuse(
            "prp-list",
            format!(
                "a transfer that fits in two PRP entries (at most 0x{:x} bytes from PRP1 0x{:x}); PRP lists are refused",
                first + PAGE,
                c.prp1
            ),
            format!("0x{len:x} bytes"),
        ));
    }
    if !c.prp2.is_multiple_of(PAGE) {
        return Err(refuse(
            "prp",
            "PRP2 page-aligned",
            format!("0x{:x}", c.prp2),
        ));
    }
    if !(ctx.driver_owns)(c.prp2, rest) {
        return Err(refuse(
            "address",
            format!(
                "PRP2 range 0x{:x}+0x{rest:x} inside one of the driver's DMA buffers",
                c.prp2
            ),
            "outside the driver's buffers",
        ));
    }
    Ok(())
}

/// QID (bits 15:0) and zero-based QSIZE (bits 31:16) from CDW10 (SPDK
/// nvme_spec.h:1203-1205), with the QID checked against `max_qid`.
fn queue_id_size(c: &Command, ctx: &Context) -> Result<(u16, u32), Refusal> {
    let qid = (c.cdw10 & 0xffff) as u16;
    if qid == 0 || qid > ctx.max_qid {
        return Err(refuse(
            "queue-id",
            format!("an I/O queue ID from 1 to {}", ctx.max_qid),
            qid.to_string(),
        ));
    }
    Ok((qid, (c.cdw10 >> 16) + 1))
}

/// Check one command a driver submitted to `queue` against `list` (rule
/// 10). Every field a rule does not name must be zero.
pub fn check(
    list: &CommandList,
    queue: QueueKind,
    c: &Command,
    ctx: &Context,
) -> Result<(&'static CommandEntry, Checked), Refusal> {
    let entry = list.find(queue, c.opcode).ok_or_else(|| {
        refuse(
            "opcode",
            format!("a {} opcode on the command list", queue.as_str()),
            format!("0x{:02x}", c.opcode),
        )
    })?;
    // FUSE = 0 (not fused), PSDT = 0 (PRPs, not SGLs), reserved bits 0.
    if c.flags != 0 {
        return Err(refuse(
            "flags",
            "CDW0 bits 15:8 = 0 (FUSE normal, PSDT PRP)",
            format!("0x{:02x}", c.flags),
        ));
    }
    zero("reserved", "CDW2", c.cdw2.into())?;
    zero("reserved", "CDW3", c.cdw3.into())?;
    zero("metadata", "metadata pointer", c.mptr)?;

    let rest_zero = |from: usize, words: &[u32]| -> Result<(), Refusal> {
        for (i, w) in words.iter().enumerate() {
            zero("unused-field", &format!("CDW{}", from + i), (*w).into())?;
        }
        Ok(())
    };

    let checked = match entry.rule {
        Rule::Identify => {
            // CDW10 is the CNS: 1 for the controller, 0 for a namespace
            // (FreeBSD nvme_ctrlr_cmd.c:49, 65). FreeBSD uses no others.
            if c.cdw10 > 1 {
                return Err(refuse(
                    "identify-cns",
                    "CDW10 = 0 (namespace) or 1 (controller)",
                    format!("0x{:08x}", c.cdw10),
                ));
            }
            rest_zero(11, &[c.cdw11, c.cdw12, c.cdw13, c.cdw14, c.cdw15])?;
            check_prps(c, PAGE, ctx)?;
            Checked::Plain
        }
        Rule::GetNumberOfQueues | Rule::SetNumberOfQueues => {
            if c.cdw10 != FID_NUMBER_OF_QUEUES {
                let what = if entry.rule == Rule::SetNumberOfQueues {
                    "CDW10 = 0x07: Number of Queues, Save (bit 31) clear"
                } else {
                    "CDW10 = 0x07: Number of Queues, current value"
                };
                return Err(refuse("feature", what, format!("0x{:08x}", c.cdw10)));
            }
            zero("unused-field", "NSID", c.nsid.into())?;
            check_prps(c, 0, ctx)?;
            if entry.rule == Rule::GetNumberOfQueues {
                rest_zero(11, &[c.cdw11])?;
            }
            rest_zero(12, &[c.cdw12, c.cdw13, c.cdw14, c.cdw15])?;
            Checked::Plain
        }
        Rule::CreateIoCq => {
            let (qid, entries) = queue_id_size(c, ctx)?;
            if !(2..=MAX_IO_CQ_ENTRIES).contains(&entries) {
                return Err(refuse(
                    "queue-size",
                    format!(
                        "2 to {MAX_IO_CQ_ENTRIES} entries (QSIZE 1 to {})",
                        MAX_IO_CQ_ENTRIES - 1
                    ),
                    format!("{entries} entries"),
                ));
            }
            // CDW11: PC bit 0, IEN bit 1, IV bits 31:16 (FreeBSD
            // nvme.h:262-263; SPDK nvme_spec.h:1318-1323).
            if c.cdw11 != 1 {
                return Err(refuse(
                    "queue-flags",
                    "CDW11 = 0x1: physically contiguous, interrupts disabled",
                    format!("0x{:08x}", c.cdw11),
                ));
            }
            zero("prp", "PRP1 (the shim allocates completion queues)", c.prp1)?;
            zero("prp", "PRP2", c.prp2)?;
            zero("unused-field", "NSID", c.nsid.into())?;
            rest_zero(12, &[c.cdw12, c.cdw13, c.cdw14, c.cdw15])?;
            Checked::CreateCq { qid, entries }
        }
        Rule::CreateIoSq => {
            let (qid, entries) = queue_id_size(c, ctx)?;
            if !(2..=MAX_IO_SQ_ENTRIES).contains(&entries) {
                return Err(refuse(
                    "queue-size",
                    format!(
                        "2 to {MAX_IO_SQ_ENTRIES} entries (QSIZE 1 to {})",
                        MAX_IO_SQ_ENTRIES - 1
                    ),
                    format!("{entries} entries"),
                ));
            }
            // CDW11: PC bit 0, QPRIO bits 2:1, CQID bits 31:16 (FreeBSD
            // nvme.h:257-259; SPDK nvme_spec.h:1308-1313).
            let cqid = (c.cdw11 >> 16) as u16;
            if c.cdw11 & 0xfff8 != 0 || c.cdw11 & 1 != 1 || cqid == 0 || cqid > ctx.max_qid {
                return Err(refuse(
                    "queue-flags",
                    format!(
                        "CDW11: physically contiguous (bit 0), a priority (bits 2:1), CQID 1 to {} (bits 31:16), nothing else",
                        ctx.max_qid
                    ),
                    format!("0x{:08x}", c.cdw11),
                ));
            }
            zero("prp", "PRP1 (the shim allocates submission queues)", c.prp1)?;
            zero("prp", "PRP2", c.prp2)?;
            zero("unused-field", "NSID", c.nsid.into())?;
            rest_zero(12, &[c.cdw12, c.cdw13, c.cdw14, c.cdw15])?;
            Checked::CreateSq { qid, entries, cqid }
        }
        Rule::DeleteIoCq | Rule::DeleteIoSq => {
            let (qid, _) = queue_id_size(c, ctx)?;
            if c.cdw10 >> 16 != 0 {
                return Err(refuse(
                    "unused-field",
                    "CDW10 bits 31:16 = 0",
                    format!("0x{:08x}", c.cdw10),
                ));
            }
            check_prps(c, 0, ctx)?;
            zero("unused-field", "NSID", c.nsid.into())?;
            rest_zero(11, &[c.cdw11, c.cdw12, c.cdw13, c.cdw14, c.cdw15])?;
            if entry.rule == Rule::DeleteIoSq {
                Checked::DeleteSq { qid }
            } else {
                Checked::DeleteCq { qid }
            }
        }
        Rule::Flush => {
            check_prps(c, 0, ctx)?;
            rest_zero(10, &[c.cdw10, c.cdw11, c.cdw12, c.cdw13, c.cdw14, c.cdw15])?;
            Checked::Plain
        }
        Rule::ReadWrite => {
            let block = match &ctx.block_bytes {
                Some(Ok(b)) => u64::from(*b),
                Some(Err(why)) => {
                    return Err(refuse(
                        "namespace",
                        format!(
                            "NSID {} names a namespace the shim can check transfers for",
                            c.nsid
                        ),
                        why.clone(),
                    ))
                }
                None => {
                    return Err(refuse(
                        "namespace",
                        format!("NSID {} identified", c.nsid),
                        "not identified",
                    ))
                }
            };
            // CDW12: NLB, zero-based, in bits 15:0 (SPDK nvme_spec.h:1391);
            // no options in the rest.
            if c.cdw12 >> 16 != 0 {
                return Err(refuse(
                    "unused-field",
                    "CDW12 bits 31:16 = 0 (no options)",
                    format!("0x{:08x}", c.cdw12),
                ));
            }
            rest_zero(13, &[c.cdw13, c.cdw14, c.cdw15])?;
            let blocks = u64::from(c.cdw12 & 0xffff) + 1;
            check_prps(c, blocks * block, ctx)?;
            Checked::Plain
        }
    };
    Ok((entry, checked))
}

// ---------------------------------------------------------------------
// The controller.

/// Register offsets (FreeBSD `struct nvme_registers`, nvme.h:817-853).
const REG_CAP: u32 = 0x00;
const REG_CC: u32 = 0x14;
const REG_CSTS: u32 = 0x1C;
const REG_AQA: u32 = 0x24;
const REG_ASQ: u32 = 0x28;
const REG_ACQ: u32 = 0x30;
const DOORBELLS: u32 = 0x1000;

/// CC fields (nvme.h:156-173): EN, CSS = NVM (0), MPS = 4 KiB (0), AMS = 0,
/// SHN = 0, IOSQES = 2^6 = 64 bytes, IOCQES = 2^4 = 16 bytes (FreeBSD
/// nvme_ctrlr.c:400-421).
const CC_EN: u32 = 1;
const CC_ENABLE: u32 = CC_EN | (6 << 16) | (4 << 20);
/// CSTS fields (nvme.h:177-180).
const CSTS_RDY: u32 = 1;
const CSTS_CFS: u32 = 1 << 1;

/// Admin queue depth the shim uses. Admin commands run one at a time.
const ADMIN_ENTRIES: u16 = 16;
/// How long the shim waits for one admin command.
const ADMIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Why an `admin` or `submit` call failed.
#[derive(Debug)]
pub enum NvmeError {
    /// Refused (rule 10). Already logged and counted.
    Refused(Refusal),
    UnknownQueue,
    QueueFull,
    /// The admin command did not complete in time. Already logged.
    Timeout,
    /// The controller or the admin queue failed earlier. Already logged.
    Failed,
}

/// Why bring-up failed: the step, what was expected, what happened.
#[derive(Debug)]
pub struct BringUpError {
    pub step: &'static str,
    pub expected: String,
    pub got: String,
}

fn bring_up_err(
    step: &'static str,
    expected: impl Into<String>,
    got: impl Into<String>,
) -> BringUpError {
    BringUpError {
        step,
        expected: expected.into(),
        got: got.into(),
    }
}

struct AdminQueue {
    sq: DmaBuffer,
    cq: DmaBuffer,
    tail: u16,
    head: u16,
    phase: bool,
}

struct IoSq {
    buf: DmaBuffer,
    entries: u16,
    tail: u16,
    /// How far the device has fetched, from the SQHD of the latest
    /// completion the shim read. Entries from here to `tail` may still be
    /// read by the device and are never overwritten.
    head: u16,
    cqid: u16,
}

struct IoCq {
    buf: DmaBuffer,
    entries: u16,
    head: u16,
    phase: bool,
}

struct Inner {
    admin: AdminQueue,
    /// A shim-owned page for the shim's own Identify commands.
    identify: DmaBuffer,
    io_cqs: BTreeMap<u16, IoCq>,
    io_sqs: BTreeMap<u16, IoSq>,
    /// Logical block size per namespace, or why the namespace is refused.
    namespaces: BTreeMap<u32, Result<u32, String>>,
    /// Set once the admin queue is unusable.
    failed: Option<String>,
    next_cid: u16,
}

/// An NVMe controller the shim has brought up.
pub struct Nvme {
    list: &'static CommandList,
    inner: Mutex<Inner>,
    refused: AtomicU32,
}

fn mmio<T>(r: Result<T, MmioError>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e:?}"))
}

fn wait_csts<R: RegionIo>(
    w: &Window<R>,
    want_rdy: bool,
    timeout: Duration,
) -> Result<u32, BringUpError> {
    let end = Instant::now() + timeout;
    loop {
        let csts = w
            .read32(REG_CSTS)
            .map_err(|e| bring_up_err("nvme-csts", "CSTS readable", format!("{e:?}")))?;
        if csts & CSTS_CFS != 0 {
            return Err(bring_up_err(
                "nvme-csts",
                "CSTS.CFS clear (no controller fatal status)",
                format!("CSTS 0x{csts:08x}"),
            ));
        }
        if (csts & CSTS_RDY != 0) == want_rdy {
            return Ok(csts);
        }
        if Instant::now() >= end {
            return Err(bring_up_err(
                "nvme-csts",
                format!(
                    "CSTS.RDY = {} within {} ms",
                    want_rdy as u8,
                    timeout.as_millis()
                ),
                format!("CSTS 0x{csts:08x}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

impl Nvme {
    /// Bring the controller up after the VFIO reset, in FreeBSD's order
    /// (nvme_ctrlr.c:289-424): disable and wait for RDY to clear, program
    /// the admin queue, enable and wait for RDY. Refuses a controller whose
    /// doorbell stride or page size the write list does not cover.
    pub fn bring_up<R: RegionIo>(
        w: &Window<R>,
        dma: &DmaAllocator,
        list: &'static CommandList,
    ) -> Result<Nvme, BringUpError> {
        let read = |off: u32| {
            w.read32(off)
                .map_err(|e| bring_up_err("nvme-cap", "CAP readable", format!("{e:?}")))
        };
        let cap_lo = read(REG_CAP)?;
        let cap_hi = read(REG_CAP + 4)?;
        // CAP fields (FreeBSD nvme.h:81-118).
        let mqes = (cap_lo & 0xffff) + 1;
        let to = (cap_lo >> 24) & 0xff;
        let dstrd = cap_hi & 0xf;
        let css = (cap_hi >> 5) & 0xff;
        let mpsmin = (cap_hi >> 16) & 0xf;
        // FreeBSD nvme_ctrlr.c:1898-1899: the wait is (CAP.TO + 1) * 500 ms.
        let ready_timeout = Duration::from_millis(u64::from(to + 1) * 500);
        if dstrd != 0 {
            return Err(bring_up_err(
                "nvme-cap",
                "CAP.DSTRD = 0 (the write list's doorbell offsets assume it)",
                format!("DSTRD {dstrd} (CAP 0x{cap_hi:08x}{cap_lo:08x})"),
            ));
        }
        if mpsmin != 0 {
            return Err(bring_up_err(
                "nvme-cap",
                "CAP.MPSMIN = 0 (4 KiB pages)",
                format!("MPSMIN {mpsmin}"),
            ));
        }
        if css & 1 == 0 {
            return Err(bring_up_err(
                "nvme-cap",
                "CAP.CSS includes the NVM command set (bit 0)",
                format!("CSS 0x{css:02x}"),
            ));
        }
        if mqes < u32::from(ADMIN_ENTRIES) {
            return Err(bring_up_err(
                "nvme-cap",
                format!("CAP.MQES + 1 >= {ADMIN_ENTRIES}"),
                format!("{mqes}"),
            ));
        }

        let wr = |off: u32, v: u32| {
            w.write32_shim(off, v).map_err(|e| {
                bring_up_err(
                    "nvme-enable",
                    format!("write 0x{v:08x} to 0x{off:02x}"),
                    format!("{e:?}"),
                )
            })
        };
        // Disable.
        let cc = w
            .read32(REG_CC)
            .map_err(|e| bring_up_err("nvme-disable", "CC readable", format!("{e:?}")))?;
        if cc & CC_EN != 0 {
            wr(REG_CC, cc & !CC_EN)?;
        }
        wait_csts(w, false, ready_timeout).map_err(|mut e| {
            e.step = "nvme-disable";
            e
        })?;

        // Admin queue, in shim-owned memory.
        let alloc = |n: usize, what: &str| {
            dma.allocate_shim(n as u32).map_err(|e| {
                bring_up_err(
                    "nvme-admin-queue",
                    format!("allocate the admin {what}"),
                    format!("{e:?}"),
                )
            })
        };
        let sq = alloc(
            usize::from(ADMIN_ENTRIES) * COMMAND_BYTES,
            "submission queue",
        )?;
        let cq = alloc(
            usize::from(ADMIN_ENTRIES) * COMPLETION_BYTES,
            "completion queue",
        )?;
        let identify = alloc(PAGE as usize, "Identify page")?;
        wr(REG_ASQ, sq.iova() as u32)?;
        wr(REG_ASQ + 4, (sq.iova() >> 32) as u32)?;
        wr(REG_ACQ, cq.iova() as u32)?;
        wr(REG_ACQ + 4, (cq.iova() >> 32) as u32)?;
        let n = u32::from(ADMIN_ENTRIES) - 1;
        wr(REG_AQA, (n << 16) | n)?;

        // Enable.
        wr(REG_CC, CC_ENABLE)?;
        let csts = wait_csts(w, true, ready_timeout)?;

        emit(
            "nvme-ready",
            &[
                (
                    "cap",
                    Value::Hex((u64::from(cap_hi) << 32) | u64::from(cap_lo)),
                ),
                ("mqes", Value::U64(mqes.into())),
                ("dstrd", Value::U64(dstrd.into())),
                ("timeout_ms", Value::U64(ready_timeout.as_millis() as u64)),
                ("cc", Value::Hex(CC_ENABLE.into())),
                ("csts", Value::Hex(csts.into())),
                ("admin_sq", Value::Hex(sq.iova())),
                ("admin_cq", Value::Hex(cq.iova())),
                ("admin_entries", Value::U64(ADMIN_ENTRIES.into())),
                ("command_list", Value::Str(list.model)),
                ("commands_reviewed", Value::Bool(list.reviewed)),
            ],
        );
        Ok(Nvme {
            list,
            inner: Mutex::new(Inner {
                admin: AdminQueue {
                    sq,
                    cq,
                    tail: 0,
                    head: 0,
                    phase: true,
                },
                identify,
                io_cqs: BTreeMap::new(),
                io_sqs: BTreeMap::new(),
                namespaces: BTreeMap::new(),
                failed: None,
                next_cid: 0,
            }),
            refused: AtomicU32::new(0),
        })
    }

    /// Number of refused commands so far. Any value above zero fails the run.
    pub fn refused(&self) -> u32 {
        self.refused.load(Ordering::SeqCst)
    }

    fn refusal(&self, queue: QueueKind, sqid: u16, c: &Command, r: Refusal) -> NvmeError {
        self.refused.fetch_add(1, Ordering::SeqCst);
        let name = if r.check == "command-size" {
            "malformed"
        } else {
            self.list
                .find(queue, c.opcode)
                .map_or("unlisted", |e| e.name)
        };
        emit(
            "command-refused",
            &[
                ("queue", Value::Str(queue.as_str())),
                ("sqid", Value::U64(sqid.into())),
                ("opcode", Value::Hex(c.opcode.into())),
                ("command", Value::Str(name)),
                ("check", Value::Str(r.check)),
                ("expected", Value::Str(&r.expected)),
                ("got", Value::Str(&r.got)),
                ("command_list", Value::Str(self.list.model)),
            ],
        );
        NvmeError::Refused(r)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run one admin command the driver submitted and return its completion
    /// entry, with the driver's command identifier.
    pub fn admin<R: RegionIo>(
        &self,
        w: &Window<R>,
        dma: &DmaAllocator,
        bytes: &[u8],
    ) -> Result<[u8; COMPLETION_BYTES], NvmeError> {
        let mut inner = self.lock();
        let c = match parse(bytes) {
            Ok(c) => c,
            Err(r) => return Err(self.refusal(QueueKind::Admin, 0, &first_byte(bytes), r)),
        };
        if inner.failed.is_some() {
            return Err(NvmeError::Failed);
        }
        let owns = |iova, len| dma.driver_owns(iova, len);
        let ctx = Context {
            driver_owns: &owns,
            block_bytes: None,
            max_qid: crate::deny::NVME_MAX_IO_QUEUES,
        };
        let (_, checked) = check(self.list, QueueKind::Admin, &c, &ctx)
            .map_err(|r| self.refusal(QueueKind::Admin, 0, &c, r))?;

        // Queue bookkeeping the device would otherwise have to catch.
        let state = match checked {
            Checked::CreateCq { qid, .. } if inner.io_cqs.contains_key(&qid) => {
                Err(format!("completion queue {qid} already exists"))
            }
            Checked::CreateSq { qid, .. } if inner.io_sqs.contains_key(&qid) => {
                Err(format!("submission queue {qid} already exists"))
            }
            Checked::CreateSq { cqid, .. } if !inner.io_cqs.contains_key(&cqid) => {
                Err(format!("completion queue {cqid} does not exist"))
            }
            Checked::DeleteSq { qid } if !inner.io_sqs.contains_key(&qid) => {
                Err(format!("submission queue {qid} does not exist"))
            }
            Checked::DeleteCq { qid } if !inner.io_cqs.contains_key(&qid) => {
                Err(format!("completion queue {qid} does not exist"))
            }
            Checked::DeleteCq { qid } if inner.io_sqs.values().any(|s| s.cqid == qid) => Err(
                format!("completion queue {qid} still has a submission queue"),
            ),
            _ => Ok(()),
        };
        if let Err(got) = state {
            return Err(self.refusal(
                QueueKind::Admin,
                0,
                &c,
                refuse(
                    "queue-state",
                    "a queue operation consistent with the queues that exist",
                    got,
                ),
            ));
        }

        // Queues live in shim memory, never the driver's.
        let mut send = c;
        let mut new_queue = None;
        let queue_bytes = match checked {
            Checked::CreateSq { entries, .. } => Some(entries * COMMAND_BYTES as u32),
            Checked::CreateCq { entries, .. } => Some(entries * COMPLETION_BYTES as u32),
            _ => None,
        };
        if let Some(bytes) = queue_bytes {
            let buf = dma
                .allocate_shim(bytes)
                .map_err(|e| self.fail(&mut inner, format!("allocate an I/O queue: {e:?}")))?;
            send.prp1 = buf.iova();
            new_queue = Some(buf);
        }
        let mut cpl = self.run_admin(&mut inner, w, send)?;
        cpl[12..14].copy_from_slice(&c.cid.to_le_bytes());

        if status_ok(&cpl) {
            match checked {
                Checked::CreateCq { qid, entries } => {
                    let buf = new_queue.take().expect("allocated above");
                    inner.io_cqs.insert(
                        qid,
                        IoCq {
                            buf,
                            entries: entries as u16,
                            head: 0,
                            phase: true,
                        },
                    );
                }
                Checked::CreateSq { qid, entries, cqid } => {
                    let buf = new_queue.take().expect("allocated above");
                    inner.io_sqs.insert(
                        qid,
                        IoSq {
                            buf,
                            entries: entries as u16,
                            tail: 0,
                            head: 0,
                            cqid,
                        },
                    );
                }
                Checked::DeleteSq { qid } => {
                    // The device no longer fetches from it.
                    inner.io_sqs.remove(&qid);
                }
                Checked::DeleteCq { qid } => {
                    // The device no longer writes to it.
                    inner.io_cqs.remove(&qid);
                }
                Checked::Plain => {}
            }
        }
        Ok(cpl)
    }

    /// Queue one I/O command on submission queue `sqid`.
    pub fn submit<R: RegionIo>(
        &self,
        w: &Window<R>,
        dma: &DmaAllocator,
        sqid: u16,
        bytes: &[u8],
    ) -> Result<(), NvmeError> {
        let mut inner = self.lock();
        let c = match parse(bytes) {
            Ok(c) => c,
            Err(r) => return Err(self.refusal(QueueKind::Io, sqid, &first_byte(bytes), r)),
        };
        if inner.failed.is_some() {
            return Err(NvmeError::Failed);
        }
        if !inner.io_sqs.contains_key(&sqid) {
            emit(
                "nvme-error",
                &[
                    ("attempted", Value::Str("submit")),
                    ("sqid", Value::U64(sqid.into())),
                    (
                        "expected",
                        Value::Str("an I/O submission queue the driver created"),
                    ),
                    ("got", Value::Str("unknown-queue")),
                ],
            );
            return Err(NvmeError::UnknownQueue);
        }
        let block_bytes =
            if self.list.find(QueueKind::Io, c.opcode).map(|e| e.rule) == Some(Rule::ReadWrite) {
                Some(self.namespace(&mut inner, w, c.nsid)?)
            } else {
                None
            };
        let owns = |iova, len| dma.driver_owns(iova, len);
        let ctx = Context {
            driver_owns: &owns,
            block_bytes,
            max_qid: crate::deny::NVME_MAX_IO_QUEUES,
        };
        check(self.list, QueueKind::Io, &c, &ctx)
            .map_err(|r| self.refusal(QueueKind::Io, sqid, &c, r))?;

        let sq = inner.io_sqs.get_mut(&sqid).expect("checked above");
        let next = (sq.tail + 1) % sq.entries;
        if next == sq.head {
            emit(
                "nvme-error",
                &[
                    ("attempted", Value::Str("submit")),
                    ("sqid", Value::U64(sqid.into())),
                    ("expected", Value::Str("a free submission queue entry")),
                    (
                        "got",
                        Value::Str(&format!(
                            "queue-full (tail {}, device head {}); collect completions first",
                            sq.tail, sq.head
                        )),
                    ),
                ],
            );
            return Err(NvmeError::QueueFull);
        }
        let slot = u32::from(sq.tail) * COMMAND_BYTES as u32;
        let written = sq.buf.write(slot, &c.encode());
        sq.tail = next;
        let tail = sq.tail;
        if let Err(e) = written {
            return Err(self.fail(
                &mut inner,
                format!("write submission queue {sqid} entry: {e:?}"),
            ));
        }
        mmio(
            w.write32_shim(sq_doorbell(sqid), u32::from(tail)),
            "SQ tail doorbell",
        )
        .map_err(|e| self.fail(&mut inner, e))?;
        Ok(())
    }

    /// Take the completions waiting in I/O completion queue `cqid`, oldest
    /// first, and return the consumed entries to the device. Each
    /// completion's SQHD tells the shim how far the device has fetched that
    /// submission queue (FreeBSD `struct nvme_completion`, nvme.h:926-940).
    pub fn complete<R: RegionIo>(
        &self,
        w: &Window<R>,
        cqid: u16,
    ) -> Result<Vec<[u8; COMPLETION_BYTES]>, NvmeError> {
        let mut inner = self.lock();
        if inner.failed.is_some() {
            return Err(NvmeError::Failed);
        }
        let Some(cq) = inner.io_cqs.get_mut(&cqid) else {
            emit(
                "nvme-error",
                &[
                    ("attempted", Value::Str("complete")),
                    ("cqid", Value::U64(cqid.into())),
                    (
                        "expected",
                        Value::Str("an I/O completion queue the driver created"),
                    ),
                    ("got", Value::Str("unknown-queue")),
                ],
            );
            return Err(NvmeError::UnknownQueue);
        };
        let mut out = Vec::new();
        let mut read_error = None;
        // At most one lap, so a call always ends.
        while out.len() < usize::from(cq.entries) {
            let at = u32::from(cq.head) * COMPLETION_BYTES as u32;
            let entry = match cq.buf.read(at, COMPLETION_BYTES as u32) {
                Ok(e) => e,
                Err(e) => {
                    read_error = Some(format!("read completion queue {cqid}: {e:?}"));
                    break;
                }
            };
            let mut cpl = [0u8; COMPLETION_BYTES];
            cpl.copy_from_slice(&entry);
            // Phase tag: status bit 0 (nvme.h:282-283).
            if (status(&cpl) & 1 == 1) != cq.phase {
                break;
            }
            cq.head = (cq.head + 1) % cq.entries;
            if cq.head == 0 {
                cq.phase = !cq.phase;
            }
            out.push(cpl);
        }
        let head = cq.head;
        if let Some(e) = read_error {
            return Err(self.fail(&mut inner, e));
        }
        for cpl in &out {
            let sqhd = u16::from_le_bytes([cpl[8], cpl[9]]);
            let sqid = u16::from_le_bytes([cpl[10], cpl[11]]);
            if let Some(sq) = inner.io_sqs.get_mut(&sqid) {
                if sq.cqid == cqid && sqhd < sq.entries {
                    sq.head = sqhd;
                }
            }
        }
        if !out.is_empty() {
            mmio(
                w.write32_shim(cq_doorbell(cqid), u32::from(head)),
                "CQ head doorbell",
            )
            .map_err(|e| self.fail(&mut inner, e))?;
        }
        Ok(out)
    }

    /// The logical block size of namespace `nsid`, identifying it on first
    /// use (Identify, CNS 0: FreeBSD nvme_ctrlr_cmd.c:55-70). The data
    /// layout is FreeBSD's `struct nvme_namespace_data` (nvme.h:1586-1695):
    /// NSZE at byte 0, NLBAF at 25, FLBAS at 26, LBAF[] at 128.
    fn namespace<R: RegionIo>(
        &self,
        inner: &mut Inner,
        w: &Window<R>,
        nsid: u32,
    ) -> Result<Result<u32, String>, NvmeError> {
        if let Some(r) = inner.namespaces.get(&nsid) {
            return Ok(r.clone());
        }
        let r = self.identify_namespace(inner, w, nsid)?;
        emit(
            "nvme-namespace",
            &[
                ("nsid", Value::U64(nsid.into())),
                (
                    "block_bytes",
                    match &r {
                        Ok(b) => Value::U64((*b).into()),
                        Err(_) => Value::Str("refused"),
                    },
                ),
                (
                    "reason",
                    Value::Str(r.as_ref().err().map_or("", |s| s.as_str())),
                ),
            ],
        );
        inner.namespaces.insert(nsid, r.clone());
        Ok(r)
    }

    fn identify_namespace<R: RegionIo>(
        &self,
        inner: &mut Inner,
        w: &Window<R>,
        nsid: u32,
    ) -> Result<Result<u32, String>, NvmeError> {
        // The shim's own page: the driver never names it, and the device
        // writes it only for this command.
        let cmd = Command {
            opcode: 0x06,
            nsid,
            prp1: inner.identify.iova(),
            ..Command::parse(&[0; 64])
        };
        let cpl = self.run_admin(inner, w, cmd)?;
        if !status_ok(&cpl) {
            return Ok(Err(format!(
                "Identify Namespace failed with status 0x{:04x}",
                status(&cpl)
            )));
        }
        match inner.identify.read(0, PAGE as u32) {
            Ok(d) => Ok(namespace_block_bytes(&d)),
            Err(e) => Err(self.fail(inner, format!("read Identify Namespace data: {e:?}"))),
        }
    }

    /// Mark the admin queue unusable, log why, and return `Failed`.
    fn fail(&self, inner: &mut Inner, why: String) -> NvmeError {
        emit(
            "nvme-failed",
            &[
                (
                    "expected",
                    Value::Str("the admin queue and controller keep working"),
                ),
                ("got", Value::Str(&why)),
            ],
        );
        inner.failed = Some(why);
        NvmeError::Failed
    }

    /// Submit `c` on the admin queue with a shim command identifier, wait
    /// for its completion, and return the completion entry.
    fn run_admin<R: RegionIo>(
        &self,
        inner: &mut Inner,
        w: &Window<R>,
        mut c: Command,
    ) -> Result<[u8; COMPLETION_BYTES], NvmeError> {
        let cid = inner.next_cid;
        inner.next_cid = inner.next_cid.wrapping_add(1);
        c.cid = cid;
        let a = &mut inner.admin;
        let slot = u32::from(a.tail) * COMMAND_BYTES as u32;
        let written = a.sq.write(slot, &c.encode());
        a.tail = (a.tail + 1) % ADMIN_ENTRIES;
        let tail = a.tail;
        if let Err(e) = written {
            return Err(self.fail(inner, format!("write admin submission queue entry: {e:?}")));
        }
        if let Err(e) = mmio(
            w.write32_shim(sq_doorbell(0), u32::from(tail)),
            "admin SQ tail doorbell",
        ) {
            return Err(self.fail(inner, e));
        }

        let end = Instant::now() + ADMIN_TIMEOUT;
        loop {
            let a = &mut inner.admin;
            let at = u32::from(a.head) * COMPLETION_BYTES as u32;
            let entry = match a.cq.read(at, COMPLETION_BYTES as u32) {
                Ok(e) => e,
                Err(e) => {
                    return Err(self.fail(inner, format!("read admin completion queue: {e:?}")))
                }
            };
            let mut cpl = [0u8; COMPLETION_BYTES];
            cpl.copy_from_slice(&entry);
            // Phase tag: status bit 0 (FreeBSD nvme.h:282-283).
            if (status(&cpl) & 1 == 1) == a.phase {
                a.head = (a.head + 1) % ADMIN_ENTRIES;
                if a.head == 0 {
                    a.phase = !a.phase;
                }
                let head = a.head;
                if let Err(e) = mmio(
                    w.write32_shim(cq_doorbell(0), u32::from(head)),
                    "admin CQ head doorbell",
                ) {
                    return Err(self.fail(inner, e));
                }
                let got = u16::from_le_bytes([cpl[12], cpl[13]]);
                if got != cid {
                    return Err(self.fail(
                        inner,
                        format!("admin completion for command {got}, expected {cid}"),
                    ));
                }
                return Ok(cpl);
            }
            if Instant::now() >= end {
                emit(
                    "nvme-timeout",
                    &[
                        ("opcode", Value::Hex(c.opcode.into())),
                        (
                            "expected",
                            Value::Str(&format!(
                                "an admin completion within {} ms",
                                ADMIN_TIMEOUT.as_millis()
                            )),
                        ),
                        ("got", Value::Str("none")),
                    ],
                );
                inner.failed = Some("admin command timed out".into());
                return Err(NvmeError::Timeout);
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }
}

/// Parse a driver's command bytes; the wrong length is a refusal.
fn parse(bytes: &[u8]) -> Result<Command, Refusal> {
    let b: &[u8; COMMAND_BYTES] = bytes.try_into().map_err(|_| {
        refuse(
            "command-size",
            format!("{COMMAND_BYTES} bytes"),
            format!("{} bytes", bytes.len()),
        )
    })?;
    Ok(Command::parse(b))
}

/// A malformed command, for logging: just its first byte as the opcode.
fn first_byte(bytes: &[u8]) -> Command {
    Command {
        opcode: bytes.first().copied().unwrap_or(0),
        ..Command::parse(&[0; COMMAND_BYTES])
    }
}

/// Completion status, dword 3 bits 31:16 (FreeBSD nvme.h:926-940).
fn status(cpl: &[u8; COMPLETION_BYTES]) -> u16 {
    u16::from_le_bytes([cpl[14], cpl[15]])
}

/// Status code and status code type both zero (nvme.h:284-287).
fn status_ok(cpl: &[u8; COMPLETION_BYTES]) -> bool {
    status(cpl) & 0x0ffe == 0
}

/// Submission queue y's tail doorbell with CAP.DSTRD = 0 (FreeBSD
/// nvme_qpair.c:622-623).
fn sq_doorbell(qid: u16) -> u32 {
    DOORBELLS + 8 * u32::from(qid)
}

/// Completion queue y's head doorbell with CAP.DSTRD = 0 (nvme_qpair.c:624-625).
fn cq_doorbell(qid: u16) -> u32 {
    DOORBELLS + 8 * u32::from(qid) + 4
}

/// The logical block size from Identify Namespace data, or why the shim
/// will not check transfers for this namespace.
fn namespace_block_bytes(d: &[u8]) -> Result<u32, String> {
    let nsze = le64(d, 0);
    if nsze == 0 {
        return Err("namespace size 0: not an active namespace".into());
    }
    let nlbaf = d[25];
    let flbas = d[26];
    // FreeBSD nvme_ns_data_format_index (nvme.h:2268-2279).
    let mut fmt = flbas & 0xf;
    if usize::from(nlbaf) + 1 > 16 {
        fmt |= ((flbas >> 5) & 0x3) << 4;
    }
    if fmt > nlbaf {
        return Err(format!("format index {fmt} beyond NLBAF {nlbaf}"));
    }
    let lbaf = le32(d, 128 + 4 * usize::from(fmt));
    // LBAF: MS bits 15:0, LBADS bits 23:16 (nvme.h:712-716).
    let ms = lbaf & 0xffff;
    let lbads = (lbaf >> 16) & 0xff;
    if ms != 0 {
        return Err(format!("{ms} bytes of metadata per block: not supported"));
    }
    if !(9..=12).contains(&lbads) {
        return Err(format!("block size 2^{lbads}: expected 512 to 4096 bytes"));
    }
    Ok(1 << lbads)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUF: u64 = 0x1_0000_0000;
    const BUF_LEN: u64 = 0x4000;

    /// The driver owns one 16 KiB buffer at BUF.
    fn owns(iova: u64, len: u64) -> bool {
        len > 0 && iova >= BUF && iova.checked_add(len).is_some_and(|e| e <= BUF + BUF_LEN)
    }

    fn ctx(block: Option<Result<u32, String>>) -> Context<'static> {
        Context {
            driver_owns: &owns,
            block_bytes: block,
            max_qid: 64,
        }
    }

    fn cmd(opcode: u8) -> Command {
        Command {
            opcode,
            ..Command::parse(&[0; 64])
        }
    }

    fn admin(c: &Command) -> Result<Checked, Refusal> {
        check(&NVME_COMMANDS, QueueKind::Admin, c, &ctx(None)).map(|(_, k)| k)
    }

    fn io(c: &Command) -> Result<Checked, Refusal> {
        check(&NVME_COMMANDS, QueueKind::Io, c, &ctx(Some(Ok(512)))).map(|(_, k)| k)
    }

    fn refused_by(r: Result<Checked, Refusal>) -> &'static str {
        match r {
            Ok(k) => panic!("expected a refusal, got {k:?}"),
            Err(r) => r.check,
        }
    }

    #[test]
    fn encode_parse_round_trip() {
        let c = Command {
            opcode: 0x02,
            flags: 0,
            cid: 0x1234,
            nsid: 1,
            cdw2: 0,
            cdw3: 0,
            mptr: 0,
            prp1: BUF + 0x200,
            prp2: BUF + 0x1000,
            cdw10: 7,
            cdw11: 0,
            cdw12: 15,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0xdead_beef,
        };
        let b = c.encode();
        assert_eq!(b[0], 0x02);
        assert_eq!(&b[2..4], &[0x34, 0x12]);
        assert_eq!(&b[60..64], &0xdead_beefu32.to_le_bytes());
        assert_eq!(Command::parse(&b), c);
    }

    #[test]
    fn every_listed_command_cites_a_pinned_source() {
        for e in NVME_COMMANDS.entries {
            assert!(
                e.source.contains(" at commit "),
                "{} does not cite a commit",
                e.name
            );
        }
    }

    #[test]
    fn unlisted_opcodes_are_refused() {
        // Firmware Commit, Firmware Image Download, Namespace Management,
        // Format NVM, Security Send, Sanitize, vendor-specific (C0h, FFh),
        // Get Log Page, Abort.
        for op in [0x10, 0x11, 0x0d, 0x80, 0x81, 0x84, 0xc0, 0xff, 0x02, 0x08] {
            assert_eq!(refused_by(admin(&cmd(op))), "opcode", "admin 0x{op:02x}");
        }
        // Write Uncorrectable, Compare, Write Zeroes, Dataset Management,
        // Verify, vendor-specific range.
        for op in [0x04, 0x05, 0x08, 0x09, 0x0c, 0x80, 0xff] {
            assert_eq!(refused_by(io(&cmd(op))), "opcode", "io 0x{op:02x}");
        }
    }

    #[test]
    fn fused_sgl_metadata_and_reserved_fields_are_refused() {
        let mut c = cmd(0x00); // Flush
        assert_eq!(io(&c), Ok(Checked::Plain));
        c.flags = 0x01; // FUSE first
        assert_eq!(refused_by(io(&c)), "flags");
        c.flags = 0x40; // PSDT SGL
        assert_eq!(refused_by(io(&c)), "flags");
        let mut c = cmd(0x00);
        c.mptr = BUF;
        assert_eq!(refused_by(io(&c)), "metadata");
        let mut c = cmd(0x00);
        c.cdw2 = 1;
        assert_eq!(refused_by(io(&c)), "reserved");
        let mut c = cmd(0x00);
        c.cdw10 = 1;
        assert_eq!(refused_by(io(&c)), "unused-field");
    }

    #[test]
    fn set_features_only_number_of_queues_without_save() {
        let mut c = cmd(0x09);
        c.cdw10 = 0x07;
        c.cdw11 = 0x0003_0003;
        assert_eq!(admin(&c), Ok(Checked::Plain));
        c.cdw10 = 0x07 | (1 << 31); // Save
        assert_eq!(refused_by(admin(&c)), "feature");
        c.cdw10 = 0x06; // Volatile Write Cache
        assert_eq!(refused_by(admin(&c)), "feature");
        c.cdw10 = 0x07;
        c.prp1 = BUF;
        assert_eq!(refused_by(admin(&c)), "prp");
    }

    #[test]
    fn get_features_only_number_of_queues() {
        let mut c = cmd(0x0a);
        c.cdw10 = 0x07;
        assert_eq!(admin(&c), Ok(Checked::Plain));
        c.cdw10 = 0x0e; // Timestamp
        assert_eq!(refused_by(admin(&c)), "feature");
    }

    #[test]
    fn identify_needs_a_page_inside_the_driver_buffers() {
        let mut c = cmd(0x06);
        c.cdw10 = 1;
        c.prp1 = BUF;
        assert_eq!(admin(&c), Ok(Checked::Plain));
        c.prp1 = BUF + BUF_LEN; // just past the buffer
        assert_eq!(refused_by(admin(&c)), "address");
        c.prp1 = 0;
        assert_eq!(refused_by(admin(&c)), "address");
        // Unaligned PRP1: the rest of the page must come through PRP2, and
        // PRP2 = 0 is not one of the driver's addresses.
        c.prp1 = BUF + 0x800;
        assert_eq!(refused_by(admin(&c)), "address");
        c.prp2 = BUF + 0x1000;
        assert_eq!(admin(&c), Ok(Checked::Plain));
        // Only the controller (CNS 1) and namespace (CNS 0) structures.
        c.cdw10 = 0;
        assert_eq!(admin(&c), Ok(Checked::Plain));
        for cns in [2, 3, 0x10, 0xff, 1 << 16] {
            c.cdw10 = cns;
            assert_eq!(refused_by(admin(&c)), "identify-cns", "CDW10 0x{cns:x}");
        }
        c.cdw10 = 1;
        c.cdw11 = 1;
        assert_eq!(refused_by(admin(&c)), "unused-field");
    }

    #[test]
    fn create_cq_is_allocated_by_the_shim() {
        let mut c = cmd(0x05);
        c.cdw10 = (63 << 16) | 1; // 64 entries, QID 1
        c.cdw11 = 1; // PC
        assert_eq!(
            admin(&c),
            Ok(Checked::CreateCq {
                qid: 1,
                entries: 64
            })
        );
        c.cdw10 = (255 << 16) | 1; // 256 entries: one page
        assert_eq!(
            admin(&c),
            Ok(Checked::CreateCq {
                qid: 1,
                entries: 256
            })
        );
        c.cdw10 = (256 << 16) | 1;
        assert_eq!(refused_by(admin(&c)), "queue-size");
        c.cdw10 = 1; // one entry
        assert_eq!(refused_by(admin(&c)), "queue-size");
        c.cdw10 = (63 << 16) | 1;
        c.prp1 = BUF; // the driver may not place it, even in its own buffer
        assert_eq!(refused_by(admin(&c)), "prp");
        c.prp1 = 0;
        c.cdw11 = 3; // interrupts on
        assert_eq!(refused_by(admin(&c)), "queue-flags");
        c.cdw11 = 0; // not contiguous
        assert_eq!(refused_by(admin(&c)), "queue-flags");
        c.cdw11 = 1;
        c.cdw10 = 63 << 16; // QID 0 is the admin queue
        assert_eq!(refused_by(admin(&c)), "queue-id");
        c.cdw10 = (63 << 16) | 65;
        assert_eq!(refused_by(admin(&c)), "queue-id");
    }

    #[test]
    fn create_sq_is_allocated_by_the_shim() {
        let mut c = cmd(0x01);
        c.cdw10 = (63 << 16) | 1;
        c.cdw11 = (1 << 16) | 1; // CQ 1, contiguous
        assert_eq!(
            admin(&c),
            Ok(Checked::CreateSq {
                qid: 1,
                entries: 64,
                cqid: 1
            })
        );
        c.prp1 = BUF; // the driver may not place it
        assert_eq!(refused_by(admin(&c)), "prp");
        c.prp1 = 0;
        c.cdw10 = (64 << 16) | 1; // 65 entries: more than a page
        assert_eq!(refused_by(admin(&c)), "queue-size");
        c.cdw10 = 1; // one entry
        assert_eq!(refused_by(admin(&c)), "queue-size");
        c.cdw10 = (63 << 16) | 1;
        c.cdw11 = 1; // CQ 0
        assert_eq!(refused_by(admin(&c)), "queue-flags");
        c.cdw11 = (1 << 16) | 1 | (1 << 3); // a reserved bit
        assert_eq!(refused_by(admin(&c)), "queue-flags");
    }

    #[test]
    fn deletes_name_an_io_queue() {
        for op in [0x00, 0x04] {
            let mut c = cmd(op);
            c.cdw10 = 2;
            assert!(admin(&c).is_ok());
            c.cdw10 = 0;
            assert_eq!(refused_by(admin(&c)), "queue-id");
            c.cdw10 = 2 | (1 << 16);
            assert_eq!(refused_by(admin(&c)), "unused-field");
        }
    }

    fn read(nlb: u32, prp1: u64, prp2: u64) -> Command {
        Command {
            nsid: 1,
            cdw12: nlb,
            prp1,
            prp2,
            ..cmd(0x02)
        }
    }

    #[test]
    fn read_write_transfers_stay_in_two_prp_entries() {
        // One 512-byte block.
        assert_eq!(io(&read(0, BUF, 0)), Ok(Checked::Plain));
        // Eight blocks: exactly one page.
        assert_eq!(io(&read(7, BUF, 0)), Ok(Checked::Plain));
        // Sixteen blocks: two pages through PRP2.
        assert_eq!(io(&read(15, BUF, BUF + 0x1000)), Ok(Checked::Plain));
        // ...but not without PRP2.
        assert_eq!(refused_by(io(&read(15, BUF, 0))), "address");
        // Seventeen blocks need a PRP list.
        assert_eq!(refused_by(io(&read(16, BUF, BUF + 0x1000))), "prp-list");
        // A one-page transfer with a stray PRP2.
        assert_eq!(refused_by(io(&read(0, BUF, BUF + 0x1000))), "prp");
        // Starting mid-page: PRP1 covers to the end of its page.
        assert_eq!(io(&read(7, BUF + 0x200, BUF + 0x1000)), Ok(Checked::Plain));
        // PRP2 must be page-aligned and inside the driver's buffers.
        assert_eq!(refused_by(io(&read(15, BUF, BUF + 0x1200))), "prp");
        assert_eq!(refused_by(io(&read(15, BUF, BUF + BUF_LEN))), "address");
        // PRP1 past the end of the buffer.
        assert_eq!(refused_by(io(&read(0, BUF + BUF_LEN, 0))), "address");
        // Options in CDW12's upper half, or anything in CDW13-15.
        assert_eq!(refused_by(io(&read(1 << 30, BUF, 0))), "unused-field");
        let mut c = read(0, BUF, 0);
        c.cdw14 = 1;
        assert_eq!(refused_by(io(&c)), "unused-field");
        // Writes follow the same rules.
        let mut w = read(16, BUF, BUF + 0x1000);
        w.opcode = 0x01;
        assert_eq!(refused_by(io(&w)), "prp-list");
    }

    #[test]
    fn read_write_need_a_known_namespace() {
        let c = read(0, BUF, 0);
        let refused = check(
            &NVME_COMMANDS,
            QueueKind::Io,
            &c,
            &ctx(Some(Err("inactive".into()))),
        );
        assert_eq!(refused.unwrap_err().check, "namespace");
        let refused = check(&NVME_COMMANDS, QueueKind::Io, &c, &ctx(None));
        assert_eq!(refused.unwrap_err().check, "namespace");
        // 4 KiB blocks: two blocks fill both PRP entries, three need a list.
        let ok = check(
            &NVME_COMMANDS,
            QueueKind::Io,
            &read(1, BUF, BUF + 0x1000),
            &ctx(Some(Ok(4096))),
        );
        assert!(ok.is_ok());
        let list = check(
            &NVME_COMMANDS,
            QueueKind::Io,
            &read(2, BUF, BUF + 0x1000),
            &ctx(Some(Ok(4096))),
        );
        assert_eq!(list.unwrap_err().check, "prp-list");
    }

    #[test]
    fn namespace_formats() {
        let mut d = vec![0u8; 4096];
        d[0] = 0x80; // NSZE 128 blocks
        d[25] = 1; // two formats
        d[26] = 1; // using format 1
        d[128..132].copy_from_slice(&(9u32 << 16).to_le_bytes()); // 512 B
        d[132..136].copy_from_slice(&(12u32 << 16).to_le_bytes()); // 4 KiB
        assert_eq!(namespace_block_bytes(&d), Ok(4096));
        d[26] = 0;
        assert_eq!(namespace_block_bytes(&d), Ok(512));
        d[128..132].copy_from_slice(&((9u32 << 16) | 8).to_le_bytes()); // 8 B metadata
        assert!(namespace_block_bytes(&d).unwrap_err().contains("metadata"));
        d[26] = 2; // beyond NLBAF
        assert!(namespace_block_bytes(&d).unwrap_err().contains("beyond"));
        d[0] = 0;
        assert!(namespace_block_bytes(&d)
            .unwrap_err()
            .contains("not an active"));
    }

    #[test]
    fn drivers_may_write_no_doorbell_and_no_controller_register() {
        assert_eq!(sq_doorbell(0), 0x1000);
        assert_eq!(cq_doorbell(0), 0x1004);
        assert_eq!(sq_doorbell(1), 0x1008);
        assert_eq!(cq_doorbell(1), 0x100c);
        // Every doorbell belongs to the shim.
        let list = &crate::deny::QEMU_NVME;
        for q in 1..=crate::deny::NVME_MAX_IO_QUEUES {
            assert!(list.check_write(cq_doorbell(q), 4).is_err(), "CQ {q} head");
            assert!(list.check_write(sq_doorbell(q), 4).is_err(), "SQ {q} tail");
        }
        assert!(
            list.check_write(cq_doorbell(0), 4).is_err(),
            "admin CQ head"
        );
        assert!(
            list.check_write(sq_doorbell(0), 4).is_err(),
            "admin SQ tail"
        );
        for reg in [
            REG_CC,
            REG_AQA,
            REG_ASQ,
            REG_ASQ + 4,
            REG_ACQ,
            REG_ACQ + 4,
            0x20,
        ] {
            assert!(list.check_write(reg, 4).is_err(), "0x{reg:x}");
        }
        // Not even the interrupt masks: drivers poll.
        assert!(list.check_write(0x0c, 4).is_err(), "INTMS");
        assert!(list.check_write(0x10, 4).is_err(), "INTMC");
        assert!(list.entries.is_empty());
    }
}
