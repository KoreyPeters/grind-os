// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wasm driver for the Realtek RTL8139C(L)+ (QEMU `rtl8139`), world `nic`.
//!
//! Implements `SPEC.md`, written from the RTL8139C(L)+ datasheet Rev. 1.6;
//! section numbers below ("DS x.y") refer to it. Polling only, C+ mode
//! (64-bit descriptor rings), one RX ring and one normal-priority TX ring.

wit_bindgen::generate!({ path: "../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;
use grind::portals::{clock, dma, log, mmio};
use std::cell::RefCell;

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

fn r8(offset: u32, name: &str) -> Result<u8, String> {
    mmio::read8(offset).map_err(|e| format!("read8 {name} (0x{offset:02x}) failed: {e:?}"))
}

fn r16(offset: u32, name: &str) -> Result<u16, String> {
    mmio::read16(offset).map_err(|e| format!("read16 {name} (0x{offset:02x}) failed: {e:?}"))
}

fn r32(offset: u32, name: &str) -> Result<u32, String> {
    mmio::read32(offset).map_err(|e| format!("read32 {name} (0x{offset:02x}) failed: {e:?}"))
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
    let fields: Vec<(String, String)> = fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    log::emit(level, message, &fields);
}

fn info(message: &str, fields: &[(&str, String)]) {
    emit(log::Level::Info, message, fields);
}

fn warn(message: &str, fields: &[(&str, String)]) {
    emit(log::Level::Warn, message, fields);
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
            return Ok((clock::now_ns() - start) / 1000);
        }
        if (clock::now_ns() - start) / 1000 >= timeout_us {
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

struct Nic {
    mac: [u8; 6],
    rx_desc: dma::Buffer,
    rx_bufs: dma::Buffer,
    /// Next RX descriptor the device will fill.
    rx_next: u32,
    tx_desc: dma::Buffer,
    tx_bufs: dma::Buffer,
    /// The (never used) high-priority ring; kept alive while the device runs.
    _tx_hi: dma::Buffer,
    /// Next free TX descriptor.
    tx_next: u32,
    /// Oldest TX descriptor not yet seen done.
    tx_clean: u32,
    /// Counts for logs.
    rx_frames: u64,
    tx_frames: u64,
}

thread_local! {
    static NIC: RefCell<Option<Nic>> = const { RefCell::new(None) };
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
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

fn dword(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([d[4 * i], d[4 * i + 1], d[4 * i + 2], d[4 * i + 3]])
}

/// SPEC.md "`start` sequence".
fn start() -> Result<Nic, String> {
    // 1. Probe and identify (DS 6.8).
    let tcr = r32(reg::TCR, "TCR")?;
    if tcr == u32::MAX {
        return Err("TCR read 0xffffffff: the device is not responding".into());
    }
    if tcr & bit::TCR_HWVER_MASK != bit::TCR_HWVER_CPLUS {
        return Err(format!(
            "TCR = 0x{tcr:08x}: HWVERID (bits 30-26, 23) = 0x{:08x}, expected 0x{:08x} (RTL8139C+, DS 6.8); C+ mode needs a C+",
            tcr & bit::TCR_HWVER_MASK,
            bit::TCR_HWVER_CPLUS
        ));
    }

    // 2. Software reset (DS 6.5).
    w8(reg::CR, "CR", bit::CR_RST)?;
    let reset_us = wait_until(
        "CR.RST (bit 4) to clear after software reset",
        RESET_TIMEOUT_US,
        100,
        || Ok(r8(reg::CR, "CR")? & bit::CR_RST == 0),
        regs_now,
    )?;
    info("reset complete", &[("waited_us", reset_us.to_string()), ("tcr", format!("0x{tcr:08x}"))]);

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

    // 4. Interrupts off, pending status cleared (DS 6.6, 6.7).
    w16(reg::IMR, "IMR", 0)?;
    w16(reg::ISR, "ISR", 0xFFFF)?;

    // 5. Rings (DS 9.2).
    let rx_desc = allocate("RX descriptor ring", RING_BYTES)?;
    let rx_bufs = allocate("RX buffers", RING * BUF_SIZE)?;
    let mut ring = vec![0u8; RING_BYTES as usize];
    for i in 0..RING {
        let at = (i * DESC_SIZE) as usize;
        let addr = rx_bufs.iova() + u64::from(i * BUF_SIZE);
        ring[at..at + 4].copy_from_slice(&rx_cmd(i).to_le_bytes());
        ring[at + 8..at + 16].copy_from_slice(&addr.to_le_bytes());
    }
    rx_desc.write(0, &ring).map_err(|e| dma_err("write RX descriptors", e))?;

    let tx_desc = allocate("TX descriptor ring", RING_BYTES)?;
    let tx_bufs = allocate("TX buffers", RING * BUF_SIZE)?;
    let mut ring = vec![0u8; RING_BYTES as usize];
    let last = ((RING - 1) * DESC_SIZE) as usize;
    ring[last..last + 4].copy_from_slice(&bit::D_EOR.to_le_bytes());
    tx_desc.write(0, &ring).map_err(|e| dma_err("write TX descriptors", e))?;

    let tx_hi = allocate("high-priority TX descriptor ring", RING_BYTES)?;
    tx_hi.write(0, &ring).map_err(|e| dma_err("write high-priority TX descriptors", e))?;

    // 6. Enter C+ mode first (DS 6.33): C+Tx, C+Rx, DAC.
    let cpcr = bit::CPCR_CPTX | bit::CPCR_CPRX | bit::CPCR_DAC;
    w16(reg::CPCR, "C+CR", cpcr)?;

    // 7. Enable TX and RX (DS 6.5, order per 6.33).
    w8(reg::CR, "CR", bit::CR_RE | bit::CR_TE)?;

    // 8. Ring addresses, TCR, RCR, multicast filter (DS 6, 6.8, 6.9, 6.34).
    let rx_iova = rx_desc.iova();
    let tx_iova = tx_desc.iova();
    let hi_iova = tx_hi.iova();
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
    let cr = r8(reg::CR, "CR")?;
    let cpcr_rb = r16(reg::CPCR, "C+CR")?;
    let rcr = r32(reg::RCR, "RCR")?;
    if cr & (bit::CR_RE | bit::CR_TE) != (bit::CR_RE | bit::CR_TE) {
        warn(
            "CR does not read back RE and TE",
            &[("expected", "bits 3 and 2 set (DS 6.5)".into()), ("got", format!("0x{cr:02x}"))],
        );
    }
    if cpcr_rb & cpcr != cpcr {
        warn(
            "C+CR does not read back what was written",
            &[("expected", format!("0x{cpcr:04x} set")), ("got", format!("0x{cpcr_rb:04x}"))],
        );
    }
    if rcr & bit::RCR_VALUE != bit::RCR_VALUE {
        warn(
            "RCR does not read back what was written",
            &[("expected", format!("0x{:08x} set", bit::RCR_VALUE)), ("got", format!("0x{rcr:08x}"))],
        );
    }

    // 9. Wait for link (DS 6.13).
    let link_us = wait_until(
        "MSR.LINKB (bit 2) to read 0 (link OK)",
        LINK_TIMEOUT_US,
        10_000,
        || Ok(r8(reg::MSR, "MSR")? & bit::MSR_LINKB == 0),
        regs_now,
    )?;

    info(
        "link up",
        &[
            ("mac", fmt_mac(&mac)),
            ("waited_us", link_us.to_string()),
            ("registers", regs_now()),
            ("rx_ring_iova", format!("0x{rx_iova:x}")),
            ("tx_ring_iova", format!("0x{tx_iova:x}")),
        ],
    );

    Ok(Nic {
        mac,
        rx_desc,
        rx_bufs,
        rx_next: 0,
        tx_desc,
        tx_bufs,
        _tx_hi: tx_hi,
        tx_next: 0,
        tx_clean: 0,
        rx_frames: 0,
        tx_frames: 0,
    })
}

impl Nic {
    /// Report and clear error bits in ISR (DS 6.7), so problems are visible.
    fn check_isr(&self, during: &str) -> Result<(), String> {
        let isr = r16(reg::ISR, "ISR")?;
        let bad = isr & bit::ISR_REPORT;
        if bad != 0 {
            warn(
                "ISR reports an error condition",
                &[
                    ("during", during.into()),
                    ("isr", format!("0x{isr:04x}")),
                    ("meaning", "bit15 SERR, bit6 RX FIFO overflow, bit4 RX descriptor unavailable, bit3 TX error, bit1 RX error (DS 6.7)".into()),
                    ("rx_next", self.rx_next.to_string()),
                    ("rx_frames", self.rx_frames.to_string()),
                    ("tx_frames", self.tx_frames.to_string()),
                    ("registers", regs_now()),
                ],
            );
            w16(reg::ISR, "ISR", bad)?;
        }
        Ok(())
    }

    /// SPEC.md "RX descriptor".
    fn receive(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.check_isr("receive")?;
        let slot = self.rx_next;
        let at = slot * DESC_SIZE;
        let d = self
            .rx_desc
            .read(at, DESC_SIZE)
            .map_err(|e| dma_err(&format!("read RX descriptor {slot}"), e))?;
        let status = dword(&d, 0);
        if status & bit::D_OWN != 0 {
            return Ok(None);
        }
        let raw_len = (status & bit::RX_LEN_MASK) as usize;
        let ok_flags = status & (bit::D_FS | bit::D_LS) == (bit::D_FS | bit::D_LS)
            && status & bit::RX_RES == 0;
        let len = raw_len.saturating_sub(FCS_LEN);
        let frame = if !ok_flags || !(MIN_FRAME..=MAX_FRAME).contains(&len) {
            warn(
                "dropped received frame",
                &[
                    ("descriptor", slot.to_string()),
                    ("status", format!("0x{status:08x}")),
                    ("frame_length_with_fcs", raw_len.to_string()),
                    ("expected", "OWN 0, FS and LS set, RES clear, length 18-1518 including FCS (DS 9.2.2)".into()),
                ],
            );
            None
        } else {
            self.rx_frames += 1;
            Some(
                self.rx_bufs
                    .read(slot * BUF_SIZE, len as u32)
                    .map_err(|e| dma_err(&format!("read RX buffer {slot} ({len} bytes)"), e))?,
            )
        };

        // Hand the descriptor back: dword 1 cleared, then dword 0 with OWN.
        self.rx_desc
            .write(at + 4, &[0u8; 4])
            .map_err(|e| dma_err(&format!("clear RX descriptor {slot} dword 1"), e))?;
        self.rx_desc
            .write(at, &rx_cmd(slot).to_le_bytes())
            .map_err(|e| dma_err(&format!("re-arm RX descriptor {slot}"), e))?;
        self.rx_next = (slot + 1) % RING;
        Ok(frame)
    }

    /// SPEC.md "TX descriptor".
    fn transmit(&mut self, frame: Vec<u8>) -> Result<(), String> {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) {
            return Err(format!("frame is {} bytes; expected 14 to 1514 (no FCS)", frame.len()));
        }
        let mut data = frame;
        if data.len() < MIN_TX_LEN {
            data.resize(MIN_TX_LEN, 0);
        }

        // Reclaim descriptors the device has finished with.
        while self.tx_clean != self.tx_next {
            let d = self
                .tx_desc
                .read(self.tx_clean * DESC_SIZE, 4)
                .map_err(|e| dma_err(&format!("read TX descriptor {}", self.tx_clean), e))?;
            let status = dword(&d, 0);
            if status & bit::D_OWN != 0 {
                break;
            }
            if status & bit::TX_TES != 0 {
                warn(
                    "transmit error reported in TX status descriptor",
                    &[
                        ("descriptor", self.tx_clean.to_string()),
                        ("status", format!("0x{status:08x}")),
                        ("meaning", "bit25 UNF, bit22 OWC, bit21 LNKF, bit20 EXC (DS 9.2.1)".into()),
                    ],
                );
            }
            self.tx_clean = (self.tx_clean + 1) % RING;
        }

        let slot = self.tx_next;
        let next = (slot + 1) % RING;
        if next == self.tx_clean {
            return Err(format!(
                "transmit ring full: oldest pending descriptor {} still owned by the device after {} frames; {}",
                self.tx_clean,
                self.tx_frames,
                regs_now()
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
        w8(reg::TPPOLL, "TPPoll", bit::TPPOLL_NPQ)?;
        self.tx_next = next;
        self.tx_frames += 1;
        self.check_isr("transmit")?;
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
        let m = NIC.with(|n| n.borrow().as_ref().map(|nic| nic.mac)).unwrap_or([0; 6]);
        (m[0], m[1], m[2], m[3], m[4], m[5])
    }

    fn link_up() -> bool {
        match r8(reg::MSR, "MSR") {
            Ok(m) => m & bit::MSR_LINKB == 0,
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
