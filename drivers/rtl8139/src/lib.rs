// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wasm driver for the Realtek RTL8139C(L)+ (QEMU `rtl8139`), world `nic`.
//!
//! Implements `SPEC.md`, written from the RTL8139C(L)+ datasheet Rev. 1.6;
//! section numbers below ("DS x.y") refer to it. Polling only, C+ mode
//! (64-bit descriptor rings), one RX ring and one normal-priority TX ring.
//!
//! Robustness (SPEC.md "Robustness contract"): no single read of a register
//! or of device-written memory is trusted; the driver checks the device's
//! configuration and ring position against its own and rebuilds them
//! (software reset, same DMA buffers) when they disagree or stop moving.

wit_bindgen::generate!({ path: "../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;
use grind::portals::{clock, dma, log, mmio};
use std::cell::{Cell, RefCell};

/// BAR1 register offsets (SPEC.md "Registers used", DS 6).
mod reg {
    pub const IDR0: u32 = 0x00;
    pub const MAR0: u32 = 0x08;
    pub const MAR4: u32 = 0x0C;
    pub const TNPDS_LO: u32 = 0x20;
    pub const TNPDS_HI: u32 = 0x24;
    pub const THPDS_LO: u32 = 0x28;
    pub const THPDS_HI: u32 = 0x2C;
    pub const CR: u32 = 0x37;
    pub const IMR: u32 = 0x3C;
    pub const ISR: u32 = 0x3E;
    pub const TCR: u32 = 0x40;
    pub const RCR: u32 = 0x44;
    pub const MPC: u32 = 0x4C;
    pub const MSR: u32 = 0x58;
    pub const BMCR: u32 = 0x62;
    pub const BMSR: u32 = 0x64;
    pub const TPPOLL: u32 = 0xD9;
    pub const CPCR: u32 = 0xE0;
    pub const RDSAR_LO: u32 = 0xE4;
    pub const RDSAR_HI: u32 = 0xE8;
}

/// Register bits and values (SPEC.md "Values written").
mod bit {
    pub const CR_RST: u8 = 1 << 4;
    pub const CR_RE: u8 = 1 << 3;
    pub const CR_TE: u8 = 1 << 2;

    pub const CPCR_CPTX: u16 = 1 << 0;
    pub const CPCR_CPRX: u16 = 1 << 1;
    pub const CPCR_DAC: u16 = 1 << 4;
    pub const CPCR_VALUE: u16 = CPCR_CPTX | CPCR_CPRX | CPCR_DAC;

    pub const MSR_LINKB: u8 = 1 << 2;

    pub const TPPOLL_NPQ: u8 = 1 << 6;

    /// TCR: IFG = 011, MXDMA = 111, append CRC, normal operation (DS 6.8).
    pub const TCR_VALUE: u32 = (0b11 << 24) | (0b111 << 8);
    /// TCR HWVERID bits 30-26 and bit 23 (DS 6.8).
    pub const TCR_HWVER_MASK: u32 = (0x1F << 26) | (1 << 23);
    /// RTL8139C+: 1,1,1,0,1 and bit 23 = 1.
    pub const TCR_HWVER_CPLUS: u32 = (0b11101 << 26) | (1 << 23);

    /// RCR: RXFTH = 111, MXDMA = 100, AB, APM (DS 6.9).
    pub const RCR_VALUE: u32 = (0b111 << 13) | (0b100 << 8) | (1 << 3) | (1 << 1);

    /// ISR bits worth reporting while polling (DS 6.7).
    pub const ISR_SERR: u16 = 1 << 15;
    pub const ISR_FOVW: u16 = 1 << 6;
    pub const ISR_RDU: u16 = 1 << 4;
    pub const ISR_TER: u16 = 1 << 3;
    pub const ISR_RER: u16 = 1 << 1;
    pub const ISR_REPORT: u16 = ISR_SERR | ISR_FOVW | ISR_RDU | ISR_TER | ISR_RER;

    /// Descriptor dword 0 (DS 9.2).
    pub const D_OWN: u32 = 1 << 31;
    pub const D_EOR: u32 = 1 << 30;
    pub const D_FS: u32 = 1 << 29;
    pub const D_LS: u32 = 1 << 28;
    /// TX status: error summary (DS 9.2.1).
    pub const TX_TES: u32 = 1 << 23;
    /// RX status: error summary (DS 9.2.2).
    pub const RX_RES: u32 = 1 << 20;
    pub const RX_LEN_MASK: u32 = 0x1FFF;
}

const RING: u32 = 32;
const DESC_SIZE: u32 = 16;
const RING_BYTES: u32 = RING * DESC_SIZE;
const BUF_SIZE: u32 = 2048;
const MIN_FRAME: usize = 14;
const MAX_FRAME: usize = 1514;
const FCS_LEN: usize = 4;
/// Frames are padded to this length before the CRC (SPEC.md "Padding").
const MIN_TX_LEN: usize = 60;

// Timeouts, in microseconds (assumed; SPEC.md).
const RESET_TIMEOUT_US: u64 = 100_000;
const LINK_TIMEOUT_US: u64 = 5_000_000;

// Robustness timing, in nanoseconds (assumed; SPEC.md "Robustness contract").
/// How often the configuration and ISR are checked against the device.
const HEALTH_INTERVAL_NS: u64 = 200_000_000;
/// How often, while the next RX slot is empty, the ring is scanned for
/// frames the driver has fallen out of step with.
const RX_SCAN_INTERVAL_NS: u64 = 20_000_000;
/// TX descriptors outstanding without progress for this long: rebuild.
const TX_STALL_NS: u64 = 1_500_000_000;
/// Minimum time between two rebuild attempts.
const REINIT_RETRY_NS: u64 = 500_000_000;
/// Register reads tried before a value of all-ones is believed.
const REG_TRIES: usize = 4;
/// Times a received frame is left in place because its reads disagree
/// before it is dropped.
const RX_UNSTABLE_TRIES: u32 = 4;

fn r8(offset: u32, name: &str) -> Result<u8, String> {
    mmio::read8(offset).map_err(|e| format!("read8 {name} (0x{offset:02x}) failed: {e:?}"))
}

fn r16(offset: u32, name: &str) -> Result<u16, String> {
    mmio::read16(offset).map_err(|e| format!("read16 {name} (0x{offset:02x}) failed: {e:?}"))
}

fn r32(offset: u32, name: &str) -> Result<u32, String> {
    mmio::read32(offset).map_err(|e| format!("read32 {name} (0x{offset:02x}) failed: {e:?}"))
}

/// Read a register up to `REG_TRIES` times and return the first value that
/// is not all-ones. `None` means every read was all-ones: the device may be
/// gone or wedged (a PCIe read of an absent device returns all-ones). None
/// of the registers read this way has a legitimate all-ones value (each has
/// reserved bits, DS 6.5, 6.7, 6.8, 6.9, 6.13, 6.33).
fn stable8(offset: u32, name: &str) -> Result<Option<u8>, String> {
    for _ in 0..REG_TRIES {
        let v = r8(offset, name)?;
        if v != u8::MAX {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

fn stable16(offset: u32, name: &str) -> Result<Option<u16>, String> {
    for _ in 0..REG_TRIES {
        let v = r16(offset, name)?;
        if v != u16::MAX {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

fn stable32(offset: u32, name: &str) -> Result<Option<u32>, String> {
    for _ in 0..REG_TRIES {
        let v = r32(offset, name)?;
        if v != u32::MAX {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

fn w8(offset: u32, name: &str, value: u8) -> Result<(), String> {
    mmio::write8(offset, value)
        .map_err(|e| format!("write8 {name} (0x{offset:02x}) <- 0x{value:02x} failed: {e:?}"))
}

fn w16(offset: u32, name: &str, value: u16) -> Result<(), String> {
    mmio::write16(offset, value)
        .map_err(|e| format!("write16 {name} (0x{offset:02x}) <- 0x{value:04x} failed: {e:?}"))
}

fn w32(offset: u32, name: &str, value: u32) -> Result<(), String> {
    mmio::write32(offset, value)
        .map_err(|e| format!("write32 {name} (0x{offset:02x}) <- 0x{value:08x} failed: {e:?}"))
}

fn dma_err(what: &str, e: dma::Error) -> String {
    format!("{what}: {e:?}")
}

fn emit(level: log::Level, message: &str, fields: &[(&str, String)]) {
    let fields: Vec<(String, String)> = fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    log::emit(level, message, &fields);
}

fn info(message: &str, fields: &[(&str, String)]) {
    emit(log::Level::Info, message, fields);
}

fn warn(message: &str, fields: &[(&str, String)]) {
    emit(log::Level::Warn, message, fields);
}

/// Nanoseconds since `since`; never underflows if the clock misbehaves.
fn elapsed_ns(since: u64) -> u64 {
    clock::now_ns().saturating_sub(since)
}

/// Poll `done` every `poll_us` until it returns true or `timeout_us` passes.
/// On timeout, the error says what was awaited and what `last` reports.
fn wait_until(
    what: &str,
    timeout_us: u64,
    poll_us: u32,
    mut done: impl FnMut() -> Result<bool, String>,
    last: impl Fn() -> String,
) -> Result<u64, String> {
    let start = clock::now_ns();
    loop {
        if done()? {
            return Ok(elapsed_ns(start) / 1000);
        }
        if elapsed_ns(start) / 1000 >= timeout_us {
            return Err(format!(
                "timed out after {} ms waiting for {what}; {}",
                timeout_us / 1000,
                last()
            ));
        }
        clock::sleep_us(poll_us);
    }
}

/// A one-line snapshot of the registers that matter, for error messages.
/// Single reads: a value of all-ones here may be an unreliable read.
fn regs_now() -> String {
    let mut parts = Vec::new();
    let p8 = |o: u32, n: &str| match mmio::read8(o) {
        Ok(v) => format!("{n} = 0x{v:02x}"),
        Err(e) => format!("{n} unreadable: {e:?}"),
    };
    let p16 = |o: u32, n: &str| match mmio::read16(o) {
        Ok(v) => format!("{n} = 0x{v:04x}"),
        Err(e) => format!("{n} unreadable: {e:?}"),
    };
    let p32 = |o: u32, n: &str| match mmio::read32(o) {
        Ok(v) => format!("{n} = 0x{v:08x}"),
        Err(e) => format!("{n} unreadable: {e:?}"),
    };
    parts.push(p8(reg::CR, "CR"));
    parts.push(p16(reg::CPCR, "C+CR"));
    parts.push(p16(reg::ISR, "ISR"));
    parts.push(p32(reg::TCR, "TCR"));
    parts.push(p32(reg::RCR, "RCR"));
    parts.push(p8(reg::MSR, "MSR"));
    parts.push(p16(reg::BMCR, "BMCR"));
    parts.push(p16(reg::BMSR, "BMSR"));
    parts.push(p32(reg::MPC, "MPC"));
    parts.join(", ")
}

/// Read `len` bytes of DMA memory until two reads agree (at most three
/// reads). `None` if no two agree: the memory cannot be trusted right now.
fn read_agreed(
    buf: &dma::Buffer,
    offset: u32,
    len: u32,
    what: &str,
) -> Result<Option<Vec<u8>>, String> {
    let rd = || {
        buf.read(offset, len)
            .map_err(|e| dma_err(&format!("read {what}"), e))
    };
    let a = rd()?;
    let b = rd()?;
    if a == b {
        return Ok(Some(a));
    }
    let c = rd()?;
    if c == b || c == a {
        return Ok(Some(c));
    }
    Ok(None)
}

/// Rate limiting for repeated warnings: the first `FULL` of each kind are
/// logged, then one in every `EVERY`, each with its occurrence number.
#[derive(Default)]
struct Limiter {
    counts: Vec<(&'static str, u64)>,
}

impl Limiter {
    const FULL: u64 = 10;
    const EVERY: u64 = 500;

    fn warn(&mut self, kind: &'static str, fields: &[(&str, String)]) {
        let n = match self.counts.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                self.counts.push((kind, 1));
                1
            }
        };
        if n <= Self::FULL || n % Self::EVERY == 0 {
            let mut f: Vec<(&str, String)> = fields.to_vec();
            f.push(("occurrence", n.to_string()));
            warn(kind, &f);
        }
    }
}

struct Nic {
    mac: [u8; 6],
    rx_desc: dma::Buffer,
    rx_bufs: dma::Buffer,
    /// Next RX descriptor the device will fill.
    rx_next: u32,
    /// Times the frame at `rx_next` could not be read consistently.
    rx_unstable: u32,
    tx_desc: dma::Buffer,
    tx_bufs: dma::Buffer,
    /// The (never used) high-priority ring; kept alive while the device runs.
    tx_hi: dma::Buffer,
    /// Next free TX descriptor.
    tx_next: u32,
    /// Oldest TX descriptor not yet seen done.
    tx_clean: u32,
    /// Last time TX made progress (or had nothing outstanding).
    tx_progress_ns: u64,
    /// Last time the RX ring was scanned for frames out of step.
    rx_scan_ns: u64,
    /// Last configuration check.
    health_ns: u64,
    /// The device read all-ones at the last check.
    device_gone: bool,
    /// Why the device must be rebuilt, if it must.
    reinit_reason: Option<String>,
    /// Last rebuild attempt.
    reinit_ns: u64,
    reinits: u64,
    /// Counts for logs.
    rx_frames: u64,
    tx_frames: u64,
    limiter: Limiter,
}

thread_local! {
    static NIC: RefCell<Option<Nic>> = const { RefCell::new(None) };
    static LINK_UNREADABLE: Cell<u64> = const { Cell::new(0) };
}

fn with_nic<T>(call: &str, f: impl FnOnce(&mut Nic) -> Result<T, String>) -> Result<T, String> {
    NIC.with(|n| match n.borrow_mut().as_mut() {
        Some(nic) => f(nic),
        None => Err(format!("{call} called before start succeeded")),
    })
}

fn allocate(what: &str, size: u32) -> Result<dma::Buffer, String> {
    dma::allocate(size).map_err(|e| dma_err(&format!("allocate {what} ({size} bytes)"), e))
}

fn rx_cmd(slot: u32) -> u32 {
    let eor = if slot == RING - 1 { bit::D_EOR } else { 0 };
    bit::D_OWN | eor | BUF_SIZE
}

fn fmt_mac(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn dword(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([d[4 * i], d[4 * i + 1], d[4 * i + 2], d[4 * i + 3]])
}

fn qword(d: &[u8], i: usize) -> u64 {
    u64::from(dword(d, i)) | (u64::from(dword(d, i + 1)) << 32)
}

/// Software reset (DS 6.5): CR = RST, then wait for RST to clear. A read of
/// all-ones does not count as cleared.
fn reset() -> Result<u64, String> {
    w8(reg::CR, "CR", bit::CR_RST)?;
    wait_until(
        "CR.RST (bit 4) to clear after software reset",
        RESET_TIMEOUT_US,
        100,
        || Ok(matches!(stable8(reg::CR, "CR")?, Some(cr) if cr & bit::CR_RST == 0)),
        regs_now,
    )
}

/// SPEC.md "`start` sequence".
fn start() -> Result<Nic, String> {
    // 1. Probe and identify (DS 6.8).
    let tcr = stable32(reg::TCR, "TCR")?.ok_or_else(|| {
        format!("TCR read 0xffffffff {REG_TRIES} times: the device is not responding")
    })?;
    if tcr & bit::TCR_HWVER_MASK != bit::TCR_HWVER_CPLUS {
        return Err(format!(
            "TCR = 0x{tcr:08x}: HWVERID (bits 30-26, 23) = 0x{:08x}, expected 0x{:08x} (RTL8139C+, DS 6.8); C+ mode needs a C+",
            tcr & bit::TCR_HWVER_MASK,
            bit::TCR_HWVER_CPLUS
        ));
    }

    // 2. Software reset (DS 6.5).
    let reset_us = reset()?;
    info(
        "reset complete",
        &[
            ("waited_us", reset_us.to_string()),
            ("tcr", format!("0x{tcr:08x}")),
        ],
    );

    // 3. Station address (DS 6, IDR0-5).
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = r8(reg::IDR0 + i as u32, "IDR")?;
    }
    if mac == [0; 6] || mac[0] & 1 != 0 {
        return Err(format!(
            "station address {} read from IDR0-5 is zero or multicast; expected a unicast address auto-loaded from the EEPROM (DS 6)",
            fmt_mac(&mac)
        ));
    }

    // 5. Rings (DS 9.2): allocated once; `program` fills them.
    let rx_desc = allocate("RX descriptor ring", RING_BYTES)?;
    let rx_bufs = allocate("RX buffers", RING * BUF_SIZE)?;
    let tx_desc = allocate("TX descriptor ring", RING_BYTES)?;
    let tx_bufs = allocate("TX buffers", RING * BUF_SIZE)?;
    let tx_hi = allocate("high-priority TX descriptor ring", RING_BYTES)?;

    let now = clock::now_ns();
    let mut nic = Nic {
        mac,
        rx_desc,
        rx_bufs,
        rx_next: 0,
        rx_unstable: 0,
        tx_desc,
        tx_bufs,
        tx_hi,
        tx_next: 0,
        tx_clean: 0,
        tx_progress_ns: now,
        rx_scan_ns: now,
        health_ns: now,
        device_gone: false,
        reinit_reason: None,
        reinit_ns: now,
        reinits: 0,
        rx_frames: 0,
        tx_frames: 0,
        limiter: Limiter::default(),
    };
    // Steps 4 and 6-8.
    nic.program()?;

    // 9. Wait for link (DS 6.13).
    let link_us = wait_until(
        "MSR.LINKB (bit 2) to read 0 (link OK)",
        LINK_TIMEOUT_US,
        10_000,
        || Ok(matches!(stable8(reg::MSR, "MSR")?, Some(m) if m & bit::MSR_LINKB == 0)),
        regs_now,
    )?;

    info(
        "link up",
        &[
            ("mac", fmt_mac(&mac)),
            ("waited_us", link_us.to_string()),
            ("registers", regs_now()),
            ("rx_ring_iova", format!("0x{:x}", nic.rx_desc.iova())),
            ("tx_ring_iova", format!("0x{:x}", nic.tx_desc.iova())),
        ],
    );
    Ok(nic)
}

/// What a check of the device's configuration found.
enum Health {
    Ok,
    /// Every register read all-ones.
    Gone,
    /// Some reads all-ones, the rest as configured: no conclusion.
    Unknown,
    /// The device answers but is not configured as the driver left it.
    Wrong(String),
}

impl Nic {
    /// Interrupts off, rings filled, C+ mode, TX/RX on, ring addresses and
    /// configuration (SPEC.md `start` steps 4-8). Used by `start` and, after
    /// a software reset, to rebuild the device. Allocates nothing.
    fn program(&mut self) -> Result<(), String> {
        // 4. Interrupts off, pending status cleared (DS 6.6, 6.7).
        w16(reg::IMR, "IMR", 0)?;
        w16(reg::ISR, "ISR", 0xFFFF)?;

        // 5. Ring contents (DS 9.2).
        let mut ring = vec![0u8; RING_BYTES as usize];
        for i in 0..RING {
            let at = (i * DESC_SIZE) as usize;
            ring[at..at + 4].copy_from_slice(&rx_cmd(i).to_le_bytes());
            ring[at + 8..at + 16].copy_from_slice(&self.rx_buf_iova(i).to_le_bytes());
        }
        self.rx_desc
            .write(0, &ring)
            .map_err(|e| dma_err("write RX descriptors", e))?;

        let mut ring = vec![0u8; RING_BYTES as usize];
        let last = ((RING - 1) * DESC_SIZE) as usize;
        ring[last..last + 4].copy_from_slice(&bit::D_EOR.to_le_bytes());
        self.tx_desc
            .write(0, &ring)
            .map_err(|e| dma_err("write TX descriptors", e))?;
        self.tx_hi
            .write(0, &ring)
            .map_err(|e| dma_err("write high-priority TX descriptors", e))?;
        self.rx_next = 0;
        self.rx_unstable = 0;
        self.tx_next = 0;
        self.tx_clean = 0;

        // 6. Enter C+ mode first (DS 6.33): C+Tx, C+Rx, DAC.
        w16(reg::CPCR, "C+CR", bit::CPCR_VALUE)?;

        // 7. Enable TX and RX (DS 6.5, order per 6.33).
        w8(reg::CR, "CR", bit::CR_RE | bit::CR_TE)?;

        // 8. Ring addresses, TCR, RCR, multicast filter (DS 6, 6.8, 6.9, 6.34).
        let rx_iova = self.rx_desc.iova();
        let tx_iova = self.tx_desc.iova();
        let hi_iova = self.tx_hi.iova();
        w32(reg::TNPDS_LO, "TNPDS low", tx_iova as u32)?;
        w32(reg::TNPDS_HI, "TNPDS high", (tx_iova >> 32) as u32)?;
        w32(reg::THPDS_LO, "THPDS low", hi_iova as u32)?;
        w32(reg::THPDS_HI, "THPDS high", (hi_iova >> 32) as u32)?;
        w32(reg::RDSAR_LO, "RDSAR low", rx_iova as u32)?;
        w32(reg::RDSAR_HI, "RDSAR high", (rx_iova >> 32) as u32)?;
        w32(reg::TCR, "TCR", bit::TCR_VALUE)?;
        w32(reg::RCR, "RCR", bit::RCR_VALUE)?;
        w32(reg::MAR0, "MAR0-3", 0)?;
        w32(reg::MAR4, "MAR4-7", 0)?;

        // Readback check of what was configured.
        match self.health()? {
            Health::Ok => {}
            Health::Wrong(why) => warn("configuration does not read back", &[("got", why)]),
            Health::Gone | Health::Unknown => warn(
                "configuration readback: registers read all-ones",
                &[("registers", regs_now())],
            ),
        }
        let now = clock::now_ns();
        self.tx_progress_ns = now;
        self.rx_scan_ns = now;
        self.health_ns = now;
        Ok(())
    }

    fn rx_buf_iova(&self, slot: u32) -> u64 {
        self.rx_bufs.iova() + u64::from(slot * BUF_SIZE)
    }

    /// Compare CR, C+CR and RCR with what `program` wrote (DS 6.5, 6.33,
    /// 6.9). Values are read with `stable*`, so only all-ones is doubted.
    fn health(&self) -> Result<Health, String> {
        let cr = stable8(reg::CR, "CR")?;
        let cpcr = stable16(reg::CPCR, "C+CR")?;
        let rcr = stable32(reg::RCR, "RCR")?;
        if cr.is_none() && cpcr.is_none() && rcr.is_none() {
            return Ok(Health::Gone);
        }
        let mut wrong = Vec::new();
        let ret = bit::CR_RE | bit::CR_TE;
        if let Some(cr) = cr {
            if cr & ret != ret {
                wrong.push(format!(
                    "CR = 0x{cr:02x}, expected RE and TE (bits 3, 2) set (DS 6.5)"
                ));
            }
        }
        if let Some(c) = cpcr {
            if c & bit::CPCR_VALUE != bit::CPCR_VALUE {
                wrong.push(format!(
                    "C+CR = 0x{c:04x}, expected bits 0x{:04x} set (DS 6.33)",
                    bit::CPCR_VALUE
                ));
            }
        }
        if let Some(r) = rcr {
            if r & bit::RCR_VALUE != bit::RCR_VALUE {
                wrong.push(format!(
                    "RCR = 0x{r:08x}, expected bits 0x{:08x} set (DS 6.9)",
                    bit::RCR_VALUE
                ));
            }
        }
        if !wrong.is_empty() {
            return Ok(Health::Wrong(wrong.join("; ")));
        }
        if cr.is_none() || cpcr.is_none() || rcr.is_none() {
            return Ok(Health::Unknown);
        }
        Ok(Health::Ok)
    }

    /// Called at the start of every `receive` and `transmit`: every
    /// `HEALTH_INTERVAL_NS`, check the configuration and ISR; rebuild the
    /// device when it needs it. An error means the device cannot be used now.
    fn maintain(&mut self) -> Result<(), String> {
        if self.reinit_reason.is_none() && elapsed_ns(self.health_ns) < HEALTH_INTERVAL_NS {
            return Ok(());
        }
        self.health_ns = clock::now_ns();
        match self.health()? {
            Health::Gone => {
                if !self.device_gone {
                    warn(
                        "device not responding",
                        &[
                            (
                                "expected",
                                "CR, C+CR and RCR as configured (DS 6.5, 6.33, 6.9)".into(),
                            ),
                            (
                                "got",
                                format!("each read all-ones {REG_TRIES} times in a row"),
                            ),
                            (
                                "action",
                                "report errors; rebuild the device when it answers again".into(),
                            ),
                        ],
                    );
                }
                self.device_gone = true;
                self.reinit_reason =
                    Some("the device stopped responding (all-ones) and came back".into());
                return Err(format!(
                    "device not responding: CR, C+CR and RCR each read all-ones {REG_TRIES} times in a row (expected CR bits 3,2 set, C+CR 0x{:04x}, RCR 0x{:08x}); the device may be gone or wedged; it will be rebuilt when it answers",
                    bit::CPCR_VALUE,
                    bit::RCR_VALUE
                ));
            }
            Health::Wrong(why) => {
                if self.device_gone {
                    info("device answering again", &[("registers", regs_now())]);
                }
                self.device_gone = false;
                if self.reinit_reason.is_none() {
                    self.reinit_reason = Some(format!("configuration lost: {why}"));
                }
            }
            Health::Ok | Health::Unknown => {
                if self.device_gone {
                    info("device answering again", &[("registers", regs_now())]);
                }
                self.device_gone = false;
            }
        }
        if self.reinit_reason.is_some() {
            return self.reinit();
        }
        self.check_isr()
    }

    /// Software reset and `program` again, with the same DMA buffers.
    /// Frames in the rings are lost. Rate limited; while a rebuild is due
    /// but not yet allowed, calls return an error saying so.
    fn reinit(&mut self) -> Result<(), String> {
        let reason = self.reinit_reason.clone().unwrap_or_default();
        if self.reinits > 0 && elapsed_ns(self.reinit_ns) < REINIT_RETRY_NS {
            return Err(format!(
                "device rebuild pending ({reason}); retrying within {} ms",
                REINIT_RETRY_NS / 1_000_000
            ));
        }
        self.reinit_ns = clock::now_ns();
        self.reinits += 1;
        warn(
            "rebuilding the device: software reset and reprogram",
            &[
                ("reason", reason.clone()),
                ("rebuild", self.reinits.to_string()),
                ("rx_frames", self.rx_frames.to_string()),
                ("tx_frames", self.tx_frames.to_string()),
                ("registers", regs_now()),
            ],
        );
        let r = reset().and_then(|_| self.program());
        match r {
            Ok(()) => {
                self.reinit_reason = None;
                info(
                    "device rebuilt",
                    &[
                        ("rebuild", self.reinits.to_string()),
                        ("registers", regs_now()),
                    ],
                );
                Ok(())
            }
            Err(e) => Err(format!(
                "rebuilding the device ({reason}) failed: {e}; will retry"
            )),
        }
    }

    /// Report and clear error bits in ISR (DS 6.7), so problems are visible.
    /// Bits are cleared only from a value that was not all-ones.
    fn check_isr(&mut self) -> Result<(), String> {
        let Some(isr) = stable16(reg::ISR, "ISR")? else {
            return Ok(());
        };
        let bad = isr & bit::ISR_REPORT;
        if bad != 0 {
            self.limiter.warn(
                "ISR reports an error condition",
                &[
                    ("isr", format!("0x{isr:04x}")),
                    ("meaning", "bit15 SERR, bit6 RX FIFO overflow, bit4 RX descriptor unavailable, bit3 TX error, bit1 RX error (DS 6.7)".into()),
                    ("rx_next", self.rx_next.to_string()),
                    ("rx_frames", self.rx_frames.to_string()),
                    ("tx_frames", self.tx_frames.to_string()),
                ],
            );
            w16(reg::ISR, "ISR", bad)?;
        }
        Ok(())
    }

    /// Read RX descriptor `slot` until two reads agree. `None`: they never
    /// did; try again on the next call.
    fn rx_descriptor(&mut self, slot: u32) -> Result<Option<Vec<u8>>, String> {
        let d = read_agreed(
            &self.rx_desc,
            slot * DESC_SIZE,
            DESC_SIZE,
            &format!("RX descriptor {slot}"),
        )?;
        if d.is_none() {
            self.limiter.warn(
                "RX descriptor reads disagree",
                &[
                    ("descriptor", slot.to_string()),
                    (
                        "expected",
                        "two of three reads of the same 16 bytes agree".into(),
                    ),
                    (
                        "action",
                        "nothing consumed; read again on the next call".into(),
                    ),
                ],
            );
        }
        Ok(d)
    }

    /// Hand RX descriptor `slot` back to the device, whole (DS 9.2.2):
    /// dwords 1-3 first, then dword 0 with OWN.
    fn rx_rearm(&mut self, slot: u32) -> Result<(), String> {
        let at = slot * DESC_SIZE;
        let mut tail = [0u8; 12];
        tail[4..12].copy_from_slice(&self.rx_buf_iova(slot).to_le_bytes());
        self.rx_desc
            .write(at + 4, &tail)
            .map_err(|e| dma_err(&format!("rewrite RX descriptor {slot} dwords 1-3"), e))?;
        self.rx_desc
            .write(at, &rx_cmd(slot).to_le_bytes())
            .map_err(|e| dma_err(&format!("re-arm RX descriptor {slot}"), e))
    }

    /// The next slot is still the device's. If the device has meanwhile
    /// filled other slots, the driver and the device are out of step (for
    /// example after a bad read made the driver skip or re-arm a slot):
    /// move `rx_next` to the oldest filled slot, the first filled one, going
    /// forward from `rx_next`, whose predecessor is not filled.
    fn rx_resync(&mut self) -> Result<(), String> {
        if elapsed_ns(self.rx_scan_ns) < RX_SCAN_INTERVAL_NS {
            return Ok(());
        }
        self.rx_scan_ns = clock::now_ns();
        let Some(ring) = read_agreed(&self.rx_desc, 0, RING_BYTES, "RX descriptor ring")? else {
            return Ok(());
        };
        let filled: Vec<bool> = (0..RING as usize)
            .map(|i| dword(&ring, 4 * i) & bit::D_OWN == 0)
            .collect();
        let count = filled.iter().filter(|f| **f).count();
        if count == 0 {
            return Ok(());
        }
        let n = RING as usize;
        let from = self.rx_next as usize;
        let oldest = (0..n)
            .map(|k| (from + k) % n)
            .find(|&j| filled[j] && !filled[(j + n - 1) % n]);
        let Some(j) = oldest else { return Ok(()) };
        if j == from {
            // Filled since the last look: not out of step.
            return Ok(());
        }
        self.limiter.warn(
            "RX ring out of step with the device; resynchronised",
            &[
                ("driver_next", self.rx_next.to_string()),
                ("oldest_filled", j.to_string()),
                ("filled_slots", count.to_string()),
                ("expected", "the device fills descriptors in ring order starting at the driver's next one (DS 9.2)".into()),
            ],
        );
        self.rx_next = j as u32;
        self.rx_unstable = 0;
        Ok(())
    }

    /// SPEC.md "RX descriptor".
    fn receive(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.maintain()?;
        let slot = self.rx_next;
        let Some(d) = self.rx_descriptor(slot)? else {
            return Ok(None);
        };
        let status = dword(&d, 0);
        if status & bit::D_OWN != 0 {
            self.rx_resync()?;
            return Ok(None);
        }
        let addr = qword(&d, 2);
        let raw_len = (status & bit::RX_LEN_MASK) as usize;
        let ok_flags = status & (bit::D_FS | bit::D_LS) == (bit::D_FS | bit::D_LS)
            && status & bit::RX_RES == 0;
        let len = raw_len.saturating_sub(FCS_LEN);
        let expected_addr = self.rx_buf_iova(slot);
        let frame = if addr != expected_addr {
            self.limiter.warn(
                "dropped received frame: descriptor buffer address changed",
                &[
                    ("descriptor", slot.to_string()),
                    ("status", format!("0x{status:08x}")),
                    (
                        "expected",
                        format!("buffer address 0x{expected_addr:x} as written"),
                    ),
                    ("got", format!("0x{addr:x}")),
                ],
            );
            None
        } else if !ok_flags || !(MIN_FRAME..=MAX_FRAME).contains(&len) {
            self.limiter.warn(
                "dropped received frame",
                &[
                    ("descriptor", slot.to_string()),
                    ("status", format!("0x{status:08x}")),
                    ("frame_length_with_fcs", raw_len.to_string()),
                    (
                        "expected",
                        "OWN 0, FS and LS set, RES clear, length 18-1518 including FCS (DS 9.2.2)"
                            .into(),
                    ),
                ],
            );
            None
        } else {
            match read_agreed(
                &self.rx_bufs,
                slot * BUF_SIZE,
                len as u32,
                &format!("RX buffer {slot}"),
            )? {
                Some(f) => Some(f),
                None => {
                    self.rx_unstable += 1;
                    if self.rx_unstable < RX_UNSTABLE_TRIES {
                        // Leave it in place; the next call reads it again.
                        return Ok(None);
                    }
                    self.limiter.warn(
                        "dropped received frame: buffer reads never agreed",
                        &[
                            ("descriptor", slot.to_string()),
                            ("length", len.to_string()),
                            ("tries", self.rx_unstable.to_string()),
                        ],
                    );
                    None
                }
            }
        };
        if frame.is_some() {
            self.rx_frames += 1;
        }
        self.rx_rearm(slot)?;
        self.rx_unstable = 0;
        self.rx_next = (slot + 1) % RING;
        Ok(frame)
    }

    /// Advance `tx_clean` over descriptors the device has finished with.
    /// A descriptor counts as done only when two reads agree on OWN = 0.
    fn tx_reclaim(&mut self) -> Result<(), String> {
        while self.tx_clean != self.tx_next {
            let slot = self.tx_clean;
            let Some(d) = read_agreed(
                &self.tx_desc,
                slot * DESC_SIZE,
                4,
                &format!("TX descriptor {slot}"),
            )?
            else {
                break;
            };
            let status = dword(&d, 0);
            if status & bit::D_OWN != 0 {
                break;
            }
            if status & bit::TX_TES != 0 {
                self.limiter.warn(
                    "transmit error reported in TX status descriptor",
                    &[
                        ("descriptor", slot.to_string()),
                        ("status", format!("0x{status:08x}")),
                        (
                            "meaning",
                            "bit25 UNF, bit22 OWC, bit21 LNKF, bit20 EXC (DS 9.2.1)".into(),
                        ),
                    ],
                );
            }
            self.tx_clean = (slot + 1) % RING;
            self.tx_progress_ns = clock::now_ns();
        }
        if self.tx_clean == self.tx_next {
            self.tx_progress_ns = clock::now_ns();
        }
        Ok(())
    }

    /// SPEC.md "TX descriptor".
    fn transmit(&mut self, frame: Vec<u8>) -> Result<(), String> {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) {
            return Err(format!(
                "frame is {} bytes; expected 14 to 1514 (no FCS)",
                frame.len()
            ));
        }
        self.maintain()?;
        let mut data = frame;
        if data.len() < MIN_TX_LEN {
            data.resize(MIN_TX_LEN, 0);
        }

        self.tx_reclaim()?;
        if self.tx_clean != self.tx_next && elapsed_ns(self.tx_progress_ns) > TX_STALL_NS {
            let pending = (self.tx_next + RING - self.tx_clean) % RING;
            self.reinit_reason = Some(format!(
                "transmit stalled: {pending} descriptors (oldest {}) still owned by the device after {} ms",
                self.tx_clean,
                TX_STALL_NS / 1_000_000
            ));
            self.reinit()?;
        }

        let slot = self.tx_next;
        let next = (slot + 1) % RING;
        if next == self.tx_clean {
            // Remind the device there is work (DS 6.32), then report.
            w8(reg::TPPOLL, "TPPoll", bit::TPPOLL_NPQ)?;
            return Err(format!(
                "transmit ring full: oldest pending descriptor {} still owned by the device after {} frames ({} ms without progress)",
                self.tx_clean,
                self.tx_frames,
                elapsed_ns(self.tx_progress_ns) / 1_000_000
            ));
        }

        self.tx_bufs
            .write(slot * BUF_SIZE, &data)
            .map_err(|e| dma_err(&format!("write TX buffer {slot}"), e))?;
        let addr = self.tx_bufs.iova() + u64::from(slot * BUF_SIZE);
        let mut tail = [0u8; 12];
        tail[4..12].copy_from_slice(&addr.to_le_bytes());
        self.tx_desc
            .write(slot * DESC_SIZE + 4, &tail)
            .map_err(|e| dma_err(&format!("write TX descriptor {slot} dwords 1-3"), e))?;
        let eor = if slot == RING - 1 { bit::D_EOR } else { 0 };
        let cmd = bit::D_OWN | eor | bit::D_FS | bit::D_LS | data.len() as u32;
        self.tx_desc
            .write(slot * DESC_SIZE, &cmd.to_le_bytes())
            .map_err(|e| dma_err(&format!("write TX descriptor {slot} dword 0"), e))?;
        if self.tx_clean == self.tx_next {
            self.tx_progress_ns = clock::now_ns();
        }
        w8(reg::TPPOLL, "TPPoll", bit::TPPOLL_NPQ)?;
        self.tx_next = next;
        self.tx_frames += 1;
        Ok(())
    }
}

struct Driver;

impl Guest for Driver {
    fn start() -> Result<(), String> {
        let nic = start()?;
        NIC.with(|n| *n.borrow_mut() = Some(nic));
        Ok(())
    }
}

impl ethernet::Guest for Driver {
    fn mac_address() -> (u8, u8, u8, u8, u8, u8) {
        let m = NIC
            .with(|n| n.borrow().as_ref().map(|nic| nic.mac))
            .unwrap_or([0; 6]);
        (m[0], m[1], m[2], m[3], m[4], m[5])
    }

    /// MSR.LINKB == 0 (DS 6.13). All-ones on every try: the device is not
    /// answering, so the link is reported down.
    fn link_up() -> bool {
        match stable8(reg::MSR, "MSR") {
            Ok(Some(m)) => m & bit::MSR_LINKB == 0,
            Ok(None) => {
                let n = LINK_UNREADABLE.with(|c| {
                    c.set(c.get() + 1);
                    c.get()
                });
                if n <= 10 || n % 500 == 0 {
                    warn(
                        "link-up: MSR read all-ones; reporting the link down",
                        &[
                            ("tries", REG_TRIES.to_string()),
                            ("occurrence", n.to_string()),
                        ],
                    );
                }
                false
            }
            Err(e) => {
                warn("link-up: MSR unreadable", &[("error", e)]);
                false
            }
        }
    }

    fn transmit(frame: Vec<u8>) -> Result<(), String> {
        with_nic("transmit", |nic| nic.transmit(frame))
    }

    fn receive() -> Result<Option<Vec<u8>>, String> {
        with_nic("receive", |nic| nic.receive())
    }
}

export!(Driver);
