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
const WAIT_NS: u64 = 500_000_000;

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
}

thread_local! {
    static DISK: RefCell<Option<Disk>> = const { RefCell::new(None) };
}

fn info(message: &str, fields: &[(&str, String)]) {
    let f: Vec<(String, String)> = fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    log::emit(log::Level::Info, message, &f);
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
    let cpl = nvme::admin(&cmd).map_err(|e| format!("{what}: expected a completion, the nvme portal returned {e:?}"))?;
    if cpl.len() != 16 {
        return Err(format!("{what}: expected a 16-byte completion, got {} bytes", cpl.len()));
    }
    match status(&cpl) {
        (0, 0) => Ok(cpl),
        (sct, sc) => Err(format!("{what}: expected success, got status SCT 0x{sct:x} SC 0x{sc:02x}")),
    }
}

fn identify(buf: &dma::Buffer, cns: u32, nsid: u32, what: &str) -> Result<Vec<u8>, String> {
    admin(what, command(OPC_IDENTIFY, 0, nsid, buf.iova(), 0, [cns, 0, 0]))?;
    buf.read(0, PAGE).map_err(|e| format!("{what}: read data: {e:?}"))
}

fn setup() -> Result<Disk, String> {
    let cap_lo = mmio::read32(REG_CAP).map_err(|e| format!("read CAP: {e:?}"))?;
    let cap_hi = mmio::read32(REG_CAP + 4).map_err(|e| format!("read CAP: {e:?}"))?;
    let csts = mmio::read32(REG_CSTS).map_err(|e| format!("read CSTS: {e:?}"))?;
    if csts & 1 != 1 || csts & 2 != 0 {
        return Err(format!("controller state: expected CSTS.RDY = 1 and CFS = 0, got CSTS 0x{csts:08x}"));
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
    let max_transfer = if mdts == 0 { u32::MAX } else { 1u32.checked_shl(mdts + 12 + mpsmin).unwrap_or(u32::MAX) };

    let ns = identify(&page, 0, NSID, "Identify Namespace 1")?;
    let blocks = le64(&ns, 0);
    let nlbaf = ns[25];
    let flbas = ns[26];
    let mut fmt = flbas & 0xf;
    if u32::from(nlbaf) + 1 > 16 {
        fmt |= ((flbas >> 5) & 0x3) << 4;
    }
    if fmt > nlbaf {
        return Err(format!("Identify Namespace 1: format index {fmt} beyond NLBAF {nlbaf}"));
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
        return Err(format!("transfer size: blocks of {block_size} bytes exceed the {max_transfer}-byte limit"));
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
    admin("Create I/O Completion Queue 1", command(OPC_CREATE_CQ, 0, 0, 0, 0, [((entries - 1) << 16) | u32::from(QID), 1, 0]))?;
    admin(
        "Create I/O Submission Queue 1",
        command(OPC_CREATE_SQ, 0, 0, 0, 0, [((entries - 1) << 16) | u32::from(QID), (u32::from(QID) << 16) | 1, 0]),
    )?;

    let data = dma::allocate(MAX_PRP_BYTES).map_err(|e| format!("allocate the data buffer: {e:?}"))?;
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
    Ok(Disk { block_size, blocks, max_bytes, data, next_cid: 1 })
}

impl Disk {
    /// Issue one I/O command and wait for its completion.
    fn io(&mut self, what: &str, opcode: u8, lba: u64, bytes: u32) -> Result<(), String> {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1).max(1);
        let (nsid, prp1, prp2, cdw) = if opcode == OPC_FLUSH {
            (NSID, 0, 0, [0, 0, 0])
        } else {
            let iova = self.data.iova();
            let prp2 = if bytes > PAGE { iova + u64::from(PAGE) } else { 0 };
            (NSID, iova, prp2, [lba as u32, (lba >> 32) as u32, bytes / self.block_size - 1])
        };
        nvme::submit(QID, &command(opcode, cid, nsid, prp1, prp2, cdw))
            .map_err(|e| format!("{what}: expected the command queued, the nvme portal returned {e:?}"))?;
        let start = clock::now_ns();
        loop {
            let done = nvme::complete(QID).map_err(|e| format!("{what}: collect completions: {e:?}"))?;
            if let Some(cpl) = done.first() {
                if done.len() != 1 || cpl.len() != 16 {
                    return Err(format!("{what}: expected one 16-byte completion, got {} ({} bytes)", done.len(), cpl.len()));
                }
                let (sqid, got) = (le16(cpl, 10), le16(cpl, 12));
                if sqid != QID || got != cid {
                    return Err(format!("{what}: expected the completion for SQ {QID} command {cid}, got SQ {sqid} command {got}"));
                }
                return match status(cpl) {
                    (0, 0) => Ok(()),
                    (sct, sc) => Err(format!("{what}: expected success, got status SCT 0x{sct:x} SC 0x{sc:02x}")),
                };
            }
            if clock::now_ns() - start > WAIT_NS {
                return Err(format!("{what}: expected a completion within 500 ms, got none"));
            }
            clock::sleep_us(5);
        }
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
        let total = u64::from(count) * u64::from(self.block_size);
        let mut out = Vec::with_capacity(total as usize);
        let mut at = lba;
        while (out.len() as u64) < total {
            let bytes = (total - out.len() as u64).min(self.max_bytes.into()) as u32;
            self.io(&format!("Read LBA {at}"), OPC_READ, at, bytes)?;
            out.extend(self.data.read(0, bytes).map_err(|e| format!("read data buffer: {e:?}"))?);
            at += u64::from(bytes / self.block_size);
        }
        Ok(out)
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), String> {
        let bs = self.block_size as usize;
        if data.is_empty() || data.len() % bs != 0 {
            return Err(format!("write: expected a whole number of {bs}-byte blocks, got {} bytes", data.len()));
        }
        self.check_range("write", lba, (data.len() / bs) as u64)?;
        let mut at = lba;
        for chunk in data.chunks(self.max_bytes as usize) {
            self.data.write(0, chunk).map_err(|e| format!("fill data buffer: {e:?}"))?;
            self.io(&format!("Write LBA {at}"), OPC_WRITE, at, chunk.len() as u32)?;
            at += (chunk.len() / bs) as u64;
        }
        Ok(())
    }
}

fn with_disk<T>(f: impl FnOnce(&mut Disk) -> Result<T, String>) -> Result<T, String> {
    DISK.with(|d| match d.borrow_mut().as_mut() {
        Some(disk) => f(disk),
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
        with_disk(|d| d.io("Flush", OPC_FLUSH, 0, 0))
    }
}

export!(Driver);
