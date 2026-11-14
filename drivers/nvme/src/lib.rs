// SPDX-License-Identifier: AGPL-3.0-or-later

//! NVMe block driver for QEMU's emulated controller (world `block-driver`).
//! Written from SPEC.md; see PROVENANCE.md. Namespace 1, one I/O queue
//! pair, one command in flight, polling.

wit_bindgen::generate!({ path: "../../portals/wit", world: "block-driver" });

use exports::grind::portals::block;
use grind::portals::{clock, dma, log, mmio, nvme};
use std::cell::RefCell;

const REG_CAP: u32 = 0x00;
const REG_CSTS: u32 = 0x1C;
const PAGE: u32 = 4096;
/// The shim allows at most two PRP entries per command.
const MAX_PRP_BYTES: u32 = 2 * PAGE;
const NSID: u32 = 1;
const QID: u16 = 1;
const QUEUE_ENTRIES: u32 = 32;
/// How long one `block` call may spend on the device, including retries
/// and waiting for an earlier command; the host's call limit is 1 s.
const CALL_BUDGET_NS: u64 = 750_000_000;
/// Attempts per command when it fails transiently (bounded retry).
const MAX_ATTEMPTS: u32 = 4;

const OPC_FLUSH: u8 = 0x00;
const OPC_WRITE: u8 = 0x01;
const OPC_READ: u8 = 0x02;
const OPC_CREATE_SQ: u8 = 0x01;
const OPC_CREATE_CQ: u8 = 0x05;
const OPC_IDENTIFY: u8 = 0x06;
const OPC_SET_FEATURES: u8 = 0x09;
const FID_NUMBER_OF_QUEUES: u32 = 0x07;

struct Disk {
    block_size: u32,
    blocks: u64,
    /// Bytes per command: a whole number of blocks.
    max_bytes: u32,
    data: dma::Buffer,
    next_cid: u16,
    pending: Option<Pending>,
    reporter: Reporter,
}

thread_local! {
    static DISK: RefCell<Option<Disk>> = const { RefCell::new(None) };
}

fn info(message: &str, fields: &[(&str, String)]) {
    let f: Vec<(String, String)> = fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    log::emit(log::Level::Info, message, &f);
}

/// Events logged in full before the rest are summarised.
const LOG_IN_FULL: u32 = 3;
/// At most one summary per event kind this often.
const SUMMARY_NS: u64 = 1_000_000_000;

/// Recoverable I/O events (a retried command, a late completion
/// discarded): the first few logged in full, the rest counted and
/// summarised at most once a second with the latest one, so none goes
/// unreported but a hostile device cannot flood the log (the log shares
/// the serial console with the test's phase markers).
#[derive(Default)]
struct Reporter {
    kinds: Vec<Kind>,
}

struct Kind {
    message: &'static str,
    level: log::Level,
    total: u64,
    unreported: u64,
    latest: Vec<(String, String)>,
    last_summary_ns: u64,
}

impl Reporter {
    fn note(&mut self, level: log::Level, message: &'static str, fields: &[(&str, String)]) {
        let fields: Vec<(String, String)> = fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let i = match self.kinds.iter().position(|k| k.message == message) {
            Some(i) => i,
            None => {
                self.kinds.push(Kind {
                    message,
                    level,
                    total: 0,
                    unreported: 0,
                    latest: Vec::new(),
                    last_summary_ns: 0,
                });
                self.kinds.len() - 1
            }
        };
        let k = &mut self.kinds[i];
        k.total += 1;
        if k.total <= u64::from(LOG_IN_FULL) {
            log::emit(level, message, &fields);
            k.last_summary_ns = clock::now_ns();
        } else {
            k.unreported += 1;
            k.latest = fields;
        }
        self.tick();
    }

    /// Emit the summary of any kind with unreported events whose last
    /// report is a second old.
    fn tick(&mut self) {
        let now = clock::now_ns();
        for k in self
            .kinds
            .iter_mut()
            .filter(|k| k.unreported > 0 && now.saturating_sub(k.last_summary_ns) >= SUMMARY_NS)
        {
            let mut f = vec![(
                "summary".to_string(),
                format!(
                    "{} more since the last report, {} in all",
                    k.unreported, k.total
                ),
            )];
            f.extend(
                k.latest
                    .iter()
                    .map(|(a, b)| (format!("latest_{a}"), b.clone())),
            );
            log::emit(k.level, k.message, &f);
            k.unreported = 0;
            k.last_summary_ns = now;
        }
    }
}

/// A failed nvme portal call, with CSTS, which reads all-ones when the
/// device is gone.
fn portal_error(what: &str, attempted: &str, e: nvme::Error) -> String {
    let csts = match mmio::read32(REG_CSTS) {
        Ok(u32::MAX) => "CSTS reads 0xffffffff: the device looks gone".to_string(),
        Ok(v) => format!("CSTS 0x{v:08x}"),
        Err(e) => format!("CSTS unreadable: {e:?}"),
    };
    format!("{what}: {attempted}: expected the nvme portal to succeed, got {e:?} ({csts})")
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from(le32(b, at)) | (u64::from(le32(b, at + 4)) << 32)
}

/// A 64-byte command; everything not given is zero.
fn command(opcode: u8, cid: u16, nsid: u32, prp1: u64, prp2: u64, cdw: [u32; 3]) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[0] = opcode;
    b[2..4].copy_from_slice(&cid.to_le_bytes());
    b[4..8].copy_from_slice(&nsid.to_le_bytes());
    b[24..32].copy_from_slice(&prp1.to_le_bytes());
    b[32..40].copy_from_slice(&prp2.to_le_bytes());
    for (i, d) in cdw.iter().enumerate() {
        b[40 + 4 * i..44 + 4 * i].copy_from_slice(&d.to_le_bytes());
    }
    b
}

/// Status field of a completion: (SCT, SC), success when both are zero.
fn status(cpl: &[u8]) -> (u16, u16) {
    let s = le16(cpl, 14);
    ((s >> 9) & 0x7, (s >> 1) & 0xff)
}

fn admin(what: &str, cmd: Vec<u8>) -> Result<Vec<u8>, String> {
    let cpl = nvme::admin(&cmd)
        .map_err(|e| format!("{what}: expected a completion, the nvme portal returned {e:?}"))?;
    if cpl.len() != 16 {
        return Err(format!(
            "{what}: expected a 16-byte completion, got {} bytes",
            cpl.len()
        ));
    }
    let (sent, got) = (le16(&cmd, 2), le16(&cpl, 12));
    if got != sent {
        return Err(format!(
            "{what}: expected the completion for command {sent}, got command {got}"
        ));
    }
    match status(&cpl) {
        (0, 0) => Ok(cpl),
        (sct, sc) => Err(format!(
            "{what}: expected success, got status SCT 0x{sct:x} SC 0x{sc:02x}"
        )),
    }
}

fn identify(buf: &dma::Buffer, cns: u32, nsid: u32, what: &str) -> Result<Vec<u8>, String> {
    admin(
        what,
        command(OPC_IDENTIFY, 0, nsid, buf.iova(), 0, [cns, 0, 0]),
    )?;
    buf.read(0, PAGE)
        .map_err(|e| format!("{what}: read data: {e:?}"))
}

fn setup() -> Result<Disk, String> {
    let cap_lo = mmio::read32(REG_CAP).map_err(|e| format!("read CAP: {e:?}"))?;
    let cap_hi = mmio::read32(REG_CAP + 4).map_err(|e| format!("read CAP: {e:?}"))?;
    let csts = mmio::read32(REG_CSTS).map_err(|e| format!("read CSTS: {e:?}"))?;
    if csts & 1 != 1 || csts & 2 != 0 {
        return Err(format!(
            "controller state: expected CSTS.RDY = 1 and CFS = 0, got CSTS 0x{csts:08x}"
        ));
    }
    let mqes = (cap_lo & 0xffff) + 1;
    let mpsmin = (cap_hi >> 16) & 0xf;

    let page = dma::allocate(PAGE).map_err(|e| format!("allocate the Identify buffer: {e:?}"))?;
    let ctrl = identify(&page, 1, 0, "Identify Controller")?;
    let mdts = u32::from(ctrl[77]);
    let nn = le32(&ctrl, 516);
    if nn < 1 {
        return Err("Identify Controller: expected at least one namespace (NN >= 1), got 0".into());
    }
    let max_transfer = if mdts == 0 {
        u32::MAX
    } else {
        1u32.checked_shl(mdts + 12 + mpsmin).unwrap_or(u32::MAX)
    };

    let ns = identify(&page, 0, NSID, "Identify Namespace 1")?;
    let blocks = le64(&ns, 0);
    let nlbaf = ns[25];
    let flbas = ns[26];
    let mut fmt = flbas & 0xf;
    if u32::from(nlbaf) + 1 > 16 {
        fmt |= ((flbas >> 5) & 0x3) << 4;
    }
    if fmt > nlbaf {
        return Err(format!(
            "Identify Namespace 1: format index {fmt} beyond NLBAF {nlbaf}"
        ));
    }
    let lbaf = le32(&ns, 128 + 4 * usize::from(fmt));
    let ms = lbaf & 0xffff;
    let lbads = (lbaf >> 16) & 0xff;
    if blocks == 0 || ms != 0 || !(9..=12).contains(&lbads) {
        return Err(format!(
            "Identify Namespace 1: expected NSZE > 0, no metadata and 512-4096-byte blocks; got NSZE {blocks}, MS {ms}, LBADS {lbads}"
        ));
    }
    let block_size = 1u32 << lbads;
    let max_bytes = (MAX_PRP_BYTES.min(max_transfer) / block_size) * block_size;
    if max_bytes == 0 {
        return Err(format!(
            "transfer size: blocks of {block_size} bytes exceed the {max_transfer}-byte limit"
        ));
    }

    let cpl = admin(
        "Set Features (Number of Queues)",
        command(OPC_SET_FEATURES, 0, 0, 0, 0, [FID_NUMBER_OF_QUEUES, 0, 0]),
    )?;
    // DW0: SQs granted in bits 15:0, CQs in 31:16, zero-based, so at least
    // one of each is always granted.
    let granted = le32(&cpl, 0);
    let (sqs, cqs) = ((granted & 0xffff) + 1, (granted >> 16) + 1);

    let entries = QUEUE_ENTRIES.min(mqes);
    admin(
        "Create I/O Completion Queue 1",
        command(
            OPC_CREATE_CQ,
            0,
            0,
            0,
            0,
            [((entries - 1) << 16) | u32::from(QID), 1, 0],
        ),
    )?;
    admin(
        "Create I/O Submission Queue 1",
        command(
            OPC_CREATE_SQ,
            0,
            0,
            0,
            0,
            [
                ((entries - 1) << 16) | u32::from(QID),
                (u32::from(QID) << 16) | 1,
                0,
            ],
        ),
    )?;

    let data =
        dma::allocate(MAX_PRP_BYTES).map_err(|e| format!("allocate the data buffer: {e:?}"))?;
    info(
        "nvme ready",
        &[
            ("block_size", block_size.to_string()),
            ("blocks", blocks.to_string()),
            ("mdts", mdts.to_string()),
            ("max_bytes_per_command", max_bytes.to_string()),
            ("queue_entries", entries.to_string()),
            ("io_queues_granted", format!("{sqs} SQ, {cqs} CQ")),
        ],
    );
    Ok(Disk {
        block_size,
        blocks,
        max_bytes,
        data,
        next_cid: 1,
        pending: None,
        reporter: Reporter::default(),
    })
}

/// The one command the device may still be working on: submitted, but
/// its completion entry not yet collected. While it exists no new command
/// is submitted, so its data buffer is never reused under it.
struct Pending {
    cid: u16,
    what: String,
    since_ns: u64,
}

/// How one attempt at a command ended.
enum Attempt {
    Done,
    /// The device reported an error; `retry` when DNR is clear.
    DeviceError {
        message: String,
        retry: bool,
    },
    /// A completion that cannot be this command's (wrong CID, SQID or
    /// size, or more entries than commands outstanding): the device
    /// returned nonsense. Every command has completed, so a retry is safe.
    Nonsense(String),
}

impl Disk {
    /// Milliseconds since `since`, for messages.
    fn elapsed_ms(since: u64) -> u64 {
        clock::now_ns().saturating_sub(since) / 1_000_000
    }

    /// Wait (until `deadline`) for the completion of a command given up on
    /// earlier, and discard it. Every submitted command produces exactly
    /// one completion entry, so the count of entries is what tells the
    /// driver the device is done with the data buffer, not their contents.
    fn drain(&mut self, what: &str, deadline: u64) -> Result<(), String> {
        while let Some(p) = &self.pending {
            let done = nvme::complete(QID).map_err(|e| {
                portal_error(what, "collect the completion of an earlier command", e)
            })?;
            if let Some(first) = done.first() {
                let (sqid, cid) = if first.len() == 16 {
                    (le16(first, 10), le16(first, 12))
                } else {
                    (0, 0)
                };
                self.reporter.note(
                    log::Level::Warn,
                    "late completion discarded",
                    &[
                        ("command", p.what.clone()),
                        ("expected_cid", p.cid.to_string()),
                        (
                            "got",
                            format!(
                                "{} entries; first SQ {sqid} CID {cid}, {} ms after submission",
                                done.len(),
                                Self::elapsed_ms(p.since_ns)
                            ),
                        ),
                    ],
                );
                self.pending = None;
                break;
            }
            if clock::now_ns() >= deadline {
                return Err(format!(
                    "{what}: expected the device to finish an earlier command first ({}, CID {}, given up on, submitted {} ms ago); still outstanding, so its data buffer cannot be reused yet",
                    p.what,
                    p.cid,
                    Self::elapsed_ms(p.since_ns)
                ));
            }
            clock::sleep_us(5);
        }
        Ok(())
    }

    /// Submit one I/O command and wait (until `deadline`) for its
    /// completion. On a timeout the command stays pending.
    fn attempt(
        &mut self,
        what: &str,
        opcode: u8,
        lba: u64,
        bytes: u32,
        deadline: u64,
    ) -> Result<Attempt, String> {
        self.drain(what, deadline)?;
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1).max(1);
        let (prp1, prp2, cdw) = if opcode == OPC_FLUSH {
            (0, 0, [0, 0, 0])
        } else {
            let iova = self.data.iova();
            let prp2 = if bytes > PAGE {
                iova + u64::from(PAGE)
            } else {
                0
            };
            (
                iova,
                prp2,
                [lba as u32, (lba >> 32) as u32, bytes / self.block_size - 1],
            )
        };
        nvme::submit(QID, &command(opcode, cid, NSID, prp1, prp2, cdw))
            .map_err(|e| portal_error(what, "queue the command", e))?;
        let since_ns = clock::now_ns();
        self.pending = Some(Pending {
            cid,
            what: what.to_string(),
            since_ns,
        });
        loop {
            let done =
                nvme::complete(QID).map_err(|e| portal_error(what, "collect the completion", e))?;
            if !done.is_empty() {
                // One command was outstanding, so the first entry ends it,
                // whatever it says.
                self.pending = None;
                if done.len() != 1 {
                    return Ok(Attempt::Nonsense(format!(
                        "expected one completion (one command outstanding), got {}",
                        done.len()
                    )));
                }
                let cpl = &done[0];
                if cpl.len() != 16 {
                    return Ok(Attempt::Nonsense(format!(
                        "expected a 16-byte completion, got {} bytes",
                        cpl.len()
                    )));
                }
                let (sqid, got) = (le16(cpl, 10), le16(cpl, 12));
                if sqid != QID || got != cid {
                    return Ok(Attempt::Nonsense(format!(
                        "expected the completion for SQ {QID} command {cid}, got SQ {sqid} command {got}"
                    )));
                }
                let s = le16(cpl, 14);
                return Ok(match status(cpl) {
                    (0, 0) => Attempt::Done,
                    (sct, sc) => {
                        let dnr = s >> 15 & 1 == 1;
                        Attempt::DeviceError {
                            message: format!(
                                "expected success, got status SCT 0x{sct:x} SC 0x{sc:02x}{}",
                                if dnr { ", DNR set" } else { "" }
                            ),
                            retry: !dnr,
                        }
                    }
                });
            }
            if clock::now_ns() >= deadline {
                return Err(format!(
                    "{what}: expected a completion within {} ms, got none; the command stays outstanding and its completion will be discarded when it arrives",
                    Self::elapsed_ms(since_ns)
                ));
            }
            clock::sleep_us(5);
        }
    }

    /// Issue one I/O command, retrying a transient failure (an error status
    /// with DNR clear, or a completion that cannot be this command's) up to
    /// MAX_ATTEMPTS times, all before `deadline`.
    fn io(
        &mut self,
        what: &str,
        opcode: u8,
        lba: u64,
        bytes: u32,
        deadline: u64,
    ) -> Result<(), String> {
        let mut failures: Vec<String> = Vec::new();
        for n in 1..=MAX_ATTEMPTS {
            let (message, retry) = match self.attempt(what, opcode, lba, bytes, deadline)? {
                Attempt::Done => {
                    if !failures.is_empty() {
                        self.reporter.note(
                            log::Level::Info,
                            "succeeded after retry",
                            &[
                                ("command", what.to_string()),
                                ("attempt", n.to_string()),
                                ("earlier", failures.join("; ")),
                            ],
                        );
                    }
                    return Ok(());
                }
                Attempt::DeviceError { message, retry } => (message, retry),
                Attempt::Nonsense(message) => (message, true),
            };
            self.reporter.note(
                log::Level::Warn,
                "command failed",
                &[
                    ("command", what.to_string()),
                    ("attempt", n.to_string()),
                    ("got", message.clone()),
                ],
            );
            failures.push(format!("attempt {n}: {message}"));
            if !retry || clock::now_ns() >= deadline {
                break;
            }
        }
        Err(format!("{what}: {}", failures.join("; ")))
    }

    fn check_range(&self, what: &str, lba: u64, count: u64) -> Result<(), String> {
        if count == 0 || lba.checked_add(count).is_none_or(|end| end > self.blocks) {
            return Err(format!(
                "{what}: expected 1 or more blocks within the {} on the disk, got {count} from LBA {lba}",
                self.blocks
            ));
        }
        Ok(())
    }

    fn read(&mut self, lba: u64, count: u32) -> Result<Vec<u8>, String> {
        self.check_range("read", lba, count.into())?;
        let deadline = clock::now_ns() + CALL_BUDGET_NS;
        let total = u64::from(count) * u64::from(self.block_size);
        let mut out = Vec::with_capacity(total as usize);
        let mut at = lba;
        while (out.len() as u64) < total {
            let bytes = (total - out.len() as u64).min(self.max_bytes.into()) as u32;
            let what = format!(
                "Read LBA {at}-{} of read({lba}, {count})",
                at + u64::from(bytes / self.block_size) - 1
            );
            self.io(&what, OPC_READ, at, bytes, deadline)?;
            out.extend(
                self.data
                    .read(0, bytes)
                    .map_err(|e| format!("{what}: read the data buffer: {e:?}"))?,
            );
            at += u64::from(bytes / self.block_size);
        }
        Ok(out)
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), String> {
        let bs = self.block_size as usize;
        if data.is_empty() || data.len() % bs != 0 {
            return Err(format!(
                "write: expected a whole number of {bs}-byte blocks, got {} bytes",
                data.len()
            ));
        }
        self.check_range("write", lba, (data.len() / bs) as u64)?;
        let deadline = clock::now_ns() + CALL_BUDGET_NS;
        let blocks = data.len() / bs;
        let mut at = lba;
        for chunk in data.chunks(self.max_bytes as usize) {
            let what = format!(
                "Write LBA {at}-{} of write({lba}, {blocks} blocks)",
                at + (chunk.len() / bs) as u64 - 1
            );
            // The buffer is free: no command is pending once drain returns.
            self.drain(&what, deadline)?;
            self.data
                .write(0, chunk)
                .map_err(|e| format!("{what}: fill the data buffer: {e:?}"))?;
            self.io(&what, OPC_WRITE, at, chunk.len() as u32, deadline)?;
            at += (chunk.len() / bs) as u64;
        }
        Ok(())
    }
}

fn with_disk<T>(f: impl FnOnce(&mut Disk) -> Result<T, String>) -> Result<T, String> {
    DISK.with(|d| match d.borrow_mut().as_mut() {
        Some(disk) => {
            disk.reporter.tick();
            f(disk)
        }
        None => Err("the driver is not started".into()),
    })
}

struct Driver;

impl Guest for Driver {
    fn start() -> Result<(), String> {
        let disk = setup()?;
        DISK.with(|d| *d.borrow_mut() = Some(disk));
        Ok(())
    }
}

impl block::Guest for Driver {
    fn block_size() -> u32 {
        DISK.with(|d| d.borrow().as_ref().map_or(0, |d| d.block_size))
    }

    fn capacity() -> u64 {
        DISK.with(|d| d.borrow().as_ref().map_or(0, |d| d.blocks))
    }

    fn read(lba: u64, count: u32) -> Result<Vec<u8>, String> {
        with_disk(|d| d.read(lba, count))
    }

    fn write(lba: u64, data: Vec<u8>) -> Result<(), String> {
        with_disk(|d| d.write(lba, &data))
    }

    fn flush() -> Result<(), String> {
        with_disk(|d| d.io("Flush", OPC_FLUSH, 0, 0, clock::now_ns() + CALL_BUDGET_NS))
    }
}

export!(Driver);
