// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wasm driver for the Intel 82574L (QEMU `e1000e`), world `nic`.
//!
//! Implements `SPEC.md`, which is written from the Intel 82574 GbE
//! Controller Family Datasheet rev 2.5; section numbers below refer to it.
//! Polling only, legacy descriptors, one RX and one TX queue.
//!
//! Robustness (SPEC.md "Robustness contract"): no single read of a register
//! or of device-written memory is trusted; the device's head registers are
//! the authority on what it has finished; a driver/device disagreement that
//! persists is resynchronised, and lost device configuration is rebuilt by
//! a reset that reuses the buffers allocated in `start`.

wit_bindgen::generate!({ path: "../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;
use grind::portals::{clock, dma, log, mmio};
use std::cell::RefCell;
use std::collections::HashMap;

/// BAR0 register offsets (SPEC.md "Registers used").
mod reg {
    pub const CTRL: u32 = 0x00000;
    pub const STATUS: u32 = 0x00008;
    pub const FCAL: u32 = 0x00028;
    pub const FCAH: u32 = 0x0002C;
    pub const FCT: u32 = 0x00030;
    pub const ICR: u32 = 0x000C0;
    pub const IMC: u32 = 0x000D8;
    pub const RCTL: u32 = 0x00100;
    pub const TCTL: u32 = 0x00400;
    pub const TIPG: u32 = 0x00410;
    pub const RDBAL: u32 = 0x02800;
    pub const RDBAH: u32 = 0x02804;
    pub const RDLEN: u32 = 0x02808;
    pub const RDH: u32 = 0x02810;
    pub const RDT: u32 = 0x02818;
    pub const TDBAL: u32 = 0x03800;
    pub const TDBAH: u32 = 0x03804;
    pub const TDLEN: u32 = 0x03808;
    pub const TDH: u32 = 0x03810;
    pub const TDT: u32 = 0x03818;
    pub const TXDCTL: u32 = 0x03828;
    pub const MTA: u32 = 0x05200;
    pub const MTA_ENTRIES: u32 = 128;
    pub const RAL0: u32 = 0x05400;
    pub const RAH0: u32 = 0x05404;
    pub const GCR: u32 = 0x05B00;
}

/// Register bits (SPEC.md "Bits").
mod bit {
    pub const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
    pub const CTRL_RSVD_SET: u32 = 1 << 3;
    pub const CTRL_ASDE: u32 = 1 << 5;
    pub const CTRL_SLU: u32 = 1 << 6;
    pub const CTRL_FRCSPD: u32 = 1 << 11;
    pub const CTRL_FRCDPLX: u32 = 1 << 12;
    pub const CTRL_RST: u32 = 1 << 26;
    pub const CTRL_RFCE: u32 = 1 << 27;
    pub const CTRL_TFCE: u32 = 1 << 28;
    pub const CTRL_VME: u32 = 1 << 30;
    pub const CTRL_PHY_RST: u32 = 1 << 31;

    pub const STATUS_LU: u32 = 1 << 1;
    pub const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;

    pub const RCTL_EN: u32 = 1 << 1;
    pub const RCTL_BAM: u32 = 1 << 15;
    pub const RCTL_SECRC: u32 = 1 << 26;

    pub const TCTL_EN: u32 = 1 << 1;
    pub const TCTL_PSP: u32 = 1 << 3;
    pub const TCTL_CT_SHIFT: u32 = 4;
    pub const TCTL_COLD_SHIFT: u32 = 12;

    pub const TXDCTL_WTHRESH_SHIFT: u32 = 16;
    pub const TXDCTL_GRAN: u32 = 1 << 24;

    pub const RAH_AV: u32 = 1 << 31;
    pub const GCR_BIT22: u32 = 1 << 22;

    /// Descriptor status: descriptor done, end of packet.
    pub const DESC_DD: u8 = 1 << 0;
    pub const DESC_EOP: u8 = 1 << 1;
    /// Legacy TX command byte.
    pub const TXCMD_EOP: u8 = 1 << 0;
    pub const TXCMD_IFCS: u8 = 1 << 1;
    pub const TXCMD_RS: u8 = 1 << 3;
}

const RING: u32 = 32;
const DESC_SIZE: u32 = 16;
const RING_BYTES: u32 = RING * DESC_SIZE;
const BUF_SIZE: u32 = 2048;
const MIN_FRAME: usize = 14;
const MAX_FRAME: usize = 1514;
/// TCTL.PSP needs at least this many bytes in the descriptor (10.2.6.1).
const MIN_TX_LEN: usize = 17;
const TIPG_VALUE: u32 = 0x0060_2006;
const RCTL_VALUE: u32 = bit::RCTL_EN | bit::RCTL_BAM | bit::RCTL_SECRC;
const TCTL_VALUE: u32 = bit::TCTL_EN
    | bit::TCTL_PSP
    | (0x0F << bit::TCTL_CT_SHIFT)
    | (0x3F << bit::TCTL_COLD_SHIFT);

// Timeouts, in microseconds.
const MASTER_DISABLE_TIMEOUT_US: u64 = 10_000;
const RESET_TIMEOUT_US: u64 = 100_000;
const LINK_TIMEOUT_US: u64 = 5_000_000;

/// How many times a register read is tried before the device is declared
/// not responding (SPEC.md "Reading registers").
const READ_TRIES: u32 = 8;
/// How many reads `read_index` makes looking for two agreeing in-range
/// values.
const INDEX_TRIES: u32 = 12;
/// How many times a descriptor or frame is read looking for two reads that
/// agree (SPEC.md "Reading device-written memory").
const DMA_TRIES: u32 = 6;

// Health checks (SPEC.md "Noticing a stall"), in nanoseconds.
/// At most this often, an idle `receive` compares the driver's ring
/// position with RDH and RDT.
const RX_CHECK_NS: u64 = 50_000_000;
/// A disagreement between RDH and the descriptors must last this long
/// before the driver resynchronises.
const RX_RESYNC_NS: u64 = 100_000_000;
/// At most this often, `link-up` checks that the device still holds the
/// configuration the driver wrote.
const HEALTH_NS: u64 = 500_000_000;
/// A full transmit ring whose head has not moved for this long, with the
/// link up, is a stalled transmitter.
const TX_STALL_NS: u64 = 1_000_000_000;
/// A ring-position disagreement that lasts this long is not a race.
const TX_RESYNC_NS: u64 = 100_000_000;

fn now_ns() -> u64 {
    clock::now_ns()
}

/// Nanoseconds since `then`, never negative even if the clock misbehaves.
fn since(then: u64) -> u64 {
    now_ns().saturating_sub(then)
}

fn write(offset: u32, name: &str, value: u32) -> Result<(), String> {
    mmio::write32(offset, value)
        .map_err(|e| format!("write {name} (0x{offset:05x}) <- 0x{value:08x} failed: {e:?}"))
}

/// Read a register that never legitimately reads all-ones (true of every
/// register this driver reads: each has reserved bits that read 0). An
/// all-ones value is what a removed or wedged PCIe device returns, so it is
/// retried; only `READ_TRIES` all-ones reads in a row are reported, as the
/// device not responding.
fn read(offset: u32, name: &str) -> Result<u32, String> {
    for _ in 0..READ_TRIES {
        let v = mmio::read32(offset)
            .map_err(|e| format!("read {name} (0x{offset:05x}) failed: {e:?}"))?;
        if v != u32::MAX {
            return Ok(v);
        }
    }
    Err(format!(
        "device not responding: {name} (0x{offset:05x}) read 0xffffffff {READ_TRIES} times in a row; expected a value with its reserved bits clear"
    ))
}

/// Read a ring index register (RDH, RDT, TDH, TDT): two reads that agree
/// and lie inside the ring. A read outside the ring (all-ones from a device
/// that did not answer) carries no information and is skipped, not counted
/// against the comparison. The hardware may move a head between two reads,
/// so disagreeing in-range reads are retried, not reported.
fn read_index(offset: u32, name: &str) -> Result<u32, String> {
    let mut prev: Option<u32> = None;
    let mut seen = Vec::new();
    for _ in 0..INDEX_TRIES {
        let v = mmio::read32(offset)
            .map_err(|e| format!("read {name} (0x{offset:05x}) failed: {e:?}"))?;
        if v < RING {
            if prev == Some(v) {
                return Ok(v);
            }
            prev = Some(v);
        }
        seen.push(format!("0x{v:x}"));
    }
    Err(format!(
        "could not read {name} (0x{offset:05x}): expected two agreeing reads below {RING} in {INDEX_TRIES}, got {}",
        seen.join(", ")
    ))
}

fn dma_err(what: &str, e: dma::Error) -> String {
    format!("{what}: {e:?}")
}

fn emit(level: log::Level, message: &str, fields: &[(&str, String)]) {
    let fields: Vec<(String, String)> =
        fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    log::emit(level, message, &fields);
}

fn info(message: &str, fields: &[(&str, String)]) {
    emit(log::Level::Info, message, fields);
}

thread_local! {
    static WARNED: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
}

/// A warning, logged in full the first 10 times per message and then every
/// 100th time, with the count, so a fault storm cannot flood the log but is
/// never silent.
fn warn(message: &str, fields: &[(&str, String)]) {
    let n = WARNED.with(|w| {
        let mut w = w.borrow_mut();
        let n = w.entry(message.to_string()).or_insert(0);
        *n += 1;
        *n
    });
    if n <= 10 || n % 100 == 0 {
        let mut f: Vec<(&str, String)> = fields.to_vec();
        f.push(("occurrence", n.to_string()));
        emit(log::Level::Warn, message, &f);
    }
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
    let start = now_ns();
    loop {
        if done()? {
            return Ok(since(start) / 1000);
        }
        if since(start) / 1000 >= timeout_us {
            return Err(format!(
                "timed out after {} ms waiting for {what}; {}",
                timeout_us / 1000,
                last()
            ));
        }
        clock::sleep_us(poll_us);
    }
}

fn status_now() -> String {
    match read(reg::STATUS, "STATUS") {
        Ok(v) => format!("STATUS = 0x{v:08x}"),
        Err(e) => e,
    }
}

fn ctrl_now() -> String {
    match read(reg::CTRL, "CTRL") {
        Ok(v) => format!("CTRL = 0x{v:08x}"),
        Err(e) => e,
    }
}

/// Distance from `from` forward to `to` around the ring.
fn ahead(from: u32, to: u32) -> u32 {
    (to + RING - from) % RING
}

struct Nic {
    mac: [u8; 6],
    rx_desc: dma::Buffer,
    rx_bufs: dma::Buffer,
    /// Next RX descriptor the hardware will fill.
    rx_next: u32,
    tx_desc: dma::Buffer,
    tx_bufs: dma::Buffer,
    /// Next free TX descriptor.
    tx_tail: u32,
    /// Oldest TX descriptor not yet known to be finished.
    tx_clean: u32,

    /// When an idle `receive` last checked RDH and RDT.
    rx_checked_ns: u64,
    /// Since when RDH has said descriptor `rx_next` is filled while the
    /// descriptor itself says it is not.
    rx_disagree_since: Option<u64>,
    /// When `link-up` last checked the device's configuration.
    health_ns: u64,
    /// Since when the transmit ring has been full, and TDH then.
    tx_full_since: Option<(u64, u32)>,
    /// Since when TDH has lain outside the driver's pending range.
    tx_disagree_since: Option<u64>,
    /// Device re-initialisations after `start`.
    reinits: u32,
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

fn fmt_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// SPEC.md "`start` sequence".
fn start(previous: Option<Nic>) -> Result<Nic, String> {
    // 1. Probe.
    read(reg::STATUS, "STATUS")?;

    // All DMA memory is allocated before the device is configured, so a
    // failed allocation leaves the device untouched and nothing leaks
    // (dropped buffers are freed by the host). A second `start` reuses the
    // buffers of the first: DMA memory never grows after `start`.
    let (rx_desc, rx_bufs, tx_desc, tx_bufs) = match previous {
        Some(p) => (p.rx_desc, p.rx_bufs, p.tx_desc, p.tx_bufs),
        None => (
            allocate("RX descriptor ring", RING_BYTES)?,
            allocate("RX buffers", RING * BUF_SIZE)?,
            allocate("TX descriptor ring", RING_BYTES)?,
            allocate("TX buffers", RING * BUF_SIZE)?,
        ),
    };
    let mut nic = Nic {
        mac: [0; 6],
        rx_desc,
        rx_bufs,
        rx_next: 0,
        tx_desc,
        tx_bufs,
        tx_tail: 0,
        tx_clean: 0,
        rx_checked_ns: 0,
        rx_disagree_since: None,
        health_ns: 0,
        tx_full_since: None,
        tx_disagree_since: None,
        reinits: 0,
    };
    nic.configure()?;

    // 16. Wait for link (10.2.2.2).
    let link_us = wait_until(
        "STATUS.LU (bit 1) after CTRL.SLU",
        LINK_TIMEOUT_US,
        10_000,
        || Ok(read(reg::STATUS, "STATUS")? & bit::STATUS_LU != 0),
        || format!("{}, {}", status_now(), ctrl_now()),
    )?;

    info(
        "link up",
        &[
            ("mac", fmt_mac(&nic.mac)),
            ("status", status_now()),
            ("waited_us", link_us.to_string()),
            ("rx_ring_iova", format!("0x{:x}", nic.rx_desc.iova())),
            ("tx_ring_iova", format!("0x{:x}", nic.tx_desc.iova())),
        ],
    );
    Ok(nic)
}

impl Nic {
    /// SPEC.md "`start` sequence", steps 2-15: reset the device and program
    /// it to use this driver's buffers. Used by `start` and to recover a
    /// device that lost its configuration or stalled. Every wait is bounded
    /// (at most about 110 ms in all), so it fits inside one call.
    fn configure(&mut self) -> Result<(), String> {
        // 2. Mask interrupts (4.6.1).
        write(reg::IMC, "IMC", u32::MAX)?;

        // 3. Quiesce before reset (10.2.2.1, 3.1.3.10).
        write(reg::RCTL, "RCTL", 0)?;
        write(reg::TCTL, "TCTL", 0)?;
        let ctrl = read(reg::CTRL, "CTRL")?;
        write(reg::CTRL, "CTRL", ctrl | bit::CTRL_GIO_MASTER_DISABLE)?;
        wait_until(
            "STATUS.GIO_MASTER_ENABLE (bit 19) to clear after CTRL.GIO_MASTER_DISABLE",
            MASTER_DISABLE_TIMEOUT_US,
            100,
            || Ok(read(reg::STATUS, "STATUS")? & bit::STATUS_GIO_MASTER_ENABLE == 0),
            status_now,
        )?;

        // 4. Global reset (4.6.2, 10.2.2.1).
        let ctrl = read(reg::CTRL, "CTRL")?;
        write(reg::CTRL, "CTRL", ctrl | bit::CTRL_RST)?;
        clock::sleep_us(1000);
        let reset_us = wait_until(
            "CTRL.RST (bit 26) to self-clear after reset",
            RESET_TIMEOUT_US,
            1000,
            || Ok(read(reg::CTRL, "CTRL")? & bit::CTRL_RST == 0),
            ctrl_now,
        )?;

        // 5. Mask interrupts again; clear pending causes (4.6.1).
        write(reg::IMC, "IMC", u32::MAX)?;
        let _ = mmio::read32(reg::ICR);
        info("reset complete", &[("waited_us", (reset_us + 1000).to_string())]);

        // 6. General configuration (4.6.2).
        let gcr = read(reg::GCR, "GCR")?;
        write(reg::GCR, "GCR", gcr | bit::GCR_BIT22)?;
        write(reg::FCAL, "FCAL", 0)?;
        write(reg::FCAH, "FCAH", 0)?;
        write(reg::FCT, "FCT", 0)?;

        // 7. Link setup: the MAC follows the PHY (4.6.3.2).
        let ctrl = read(reg::CTRL, "CTRL")?;
        let ctrl = (ctrl | bit::CTRL_SLU | bit::CTRL_RSVD_SET)
            & !(bit::CTRL_ASDE
                | bit::CTRL_FRCSPD
                | bit::CTRL_FRCDPLX
                | bit::CTRL_RFCE
                | bit::CTRL_TFCE
                | bit::CTRL_VME
                | bit::CTRL_PHY_RST
                | bit::CTRL_GIO_MASTER_DISABLE
                | bit::CTRL_RST);
        write(reg::CTRL, "CTRL", ctrl)?;

        // 8. Station address (10.2.5.22-23), read twice: both must agree.
        let (ral, rah) = (read(reg::RAL0, "RAL0")?, read(reg::RAH0, "RAH0")?);
        let (ral2, rah2) = (read(reg::RAL0, "RAL0")?, read(reg::RAH0, "RAH0")?);
        if (ral, rah) != (ral2, rah2) {
            return Err(format!(
                "station address unstable: RAL0/RAH0 read 0x{ral:08x}/0x{rah:08x} then 0x{ral2:08x}/0x{rah2:08x}; expected the same value twice"
            ));
        }
        if rah & bit::RAH_AV == 0 {
            return Err(format!(
                "no valid station address loaded from NVM: RAH0.AV is 0 (RAL0 = 0x{ral:08x}, RAH0 = 0x{rah:08x})"
            ));
        }
        let l = ral.to_le_bytes();
        let h = rah.to_le_bytes();
        let mac = [l[0], l[1], l[2], l[3], h[0], h[1]];
        if mac == [0; 6] || mac[0] & 1 != 0 {
            return Err(format!(
                "station address {} is zero, broadcast or multicast (RAL0 = 0x{ral:08x}, RAH0 = 0x{rah:08x})",
                fmt_mac(&mac)
            ));
        }
        if self.mac != [0; 6] && self.mac != mac {
            return Err(format!(
                "station address changed across a reset: was {}, now {}",
                fmt_mac(&self.mac),
                fmt_mac(&mac)
            ));
        }
        self.mac = mac;

        // 9. Multicast table (4.6.5).
        for i in 0..reg::MTA_ENTRIES {
            write(reg::MTA + 4 * i, "MTA", 0)?;
        }

        // 10. RX ring (4.6.5, 4.6.5.1): every descriptor gets its buffer
        //     address and a zero status.
        let rx_bufs_iova = self.rx_bufs.iova();
        let mut ring = vec![0u8; RING_BYTES as usize];
        for i in 0..RING {
            let at = (i * DESC_SIZE) as usize;
            let addr = rx_bufs_iova + u64::from(i * BUF_SIZE);
            ring[at..at + 8].copy_from_slice(&addr.to_le_bytes());
        }
        self.rx_desc
            .write(0, &ring)
            .map_err(|e| dma_err("write RX descriptors", e))?;
        let rx_iova = self.rx_desc.iova();
        write(reg::RDBAL, "RDBAL", rx_iova as u32)?;
        write(reg::RDBAH, "RDBAH", (rx_iova >> 32) as u32)?;
        write(reg::RDLEN, "RDLEN", RING_BYTES)?;
        write(reg::RDH, "RDH", 0)?;
        write(reg::RDT, "RDT", RING - 1)?;
        self.rx_next = 0;

        // 11. RCTL (10.2.5.1): legacy descriptors, 2048-byte buffers.
        write(reg::RCTL, "RCTL", RCTL_VALUE)?;

        // 12. TX ring (4.6.6), all descriptors zeroed.
        self.tx_desc
            .write(0, &vec![0u8; RING_BYTES as usize])
            .map_err(|e| dma_err("write TX descriptors", e))?;
        let tx_iova = self.tx_desc.iova();
        write(reg::TDBAL, "TDBAL", tx_iova as u32)?;
        write(reg::TDBAH, "TDBAH", (tx_iova >> 32) as u32)?;
        write(reg::TDLEN, "TDLEN", RING_BYTES)?;
        write(reg::TDH, "TDH", 0)?;
        write(reg::TDT, "TDT", 0)?;
        self.tx_tail = 0;
        self.tx_clean = 0;

        // 13-15. TXDCTL, TCTL, TIPG (4.6.6, 10.2.6.2).
        write(
            reg::TXDCTL,
            "TXDCTL",
            bit::TXDCTL_GRAN | (1 << bit::TXDCTL_WTHRESH_SHIFT),
        )?;
        write(reg::TCTL, "TCTL", TCTL_VALUE)?;
        write(reg::TIPG, "TIPG", TIPG_VALUE)?;

        let now = now_ns();
        self.rx_checked_ns = now;
        self.rx_disagree_since = None;
        self.health_ns = now;
        self.tx_full_since = None;
        self.tx_disagree_since = None;
        Ok(())
    }

    /// Reset and reprogram the device after `start` (SPEC.md "Recovery").
    /// The link comes back by itself; `link-up` reports it.
    fn reinit(&mut self, why: &str) -> Result<(), String> {
        self.reinits += 1;
        warn(
            "re-initialising the device",
            &[
                ("why", why.to_string()),
                ("reinits", self.reinits.to_string()),
            ],
        );
        self.configure()
            .map_err(|e| format!("re-initialising the device after \"{why}\" failed: {e}"))
    }

    /// Read RX descriptor `slot` until two reads agree and its buffer
    /// address is the one the driver wrote (the device never writes the
    /// address, 7.1.3.1). `None` if no two reads agreed.
    fn rx_descriptor(&self, slot: u32) -> Result<Option<[u8; 16]>, String> {
        let expect_addr = (self.rx_bufs.iova() + u64::from(slot * BUF_SIZE)).to_le_bytes();
        let mut prev: Option<[u8; 16]> = None;
        for _ in 0..DMA_TRIES {
            let d = self
                .rx_desc
                .read(slot * DESC_SIZE, DESC_SIZE)
                .map_err(|e| dma_err(&format!("read RX descriptor {slot}"), e))?;
            let mut d16 = [0u8; 16];
            if d.len() != 16 {
                return Err(format!(
                    "read RX descriptor {slot}: expected 16 bytes, got {}",
                    d.len()
                ));
            }
            d16.copy_from_slice(&d);
            if d16[0..8] != expect_addr {
                prev = None;
                continue;
            }
            if prev == Some(d16) {
                return Ok(Some(d16));
            }
            prev = Some(d16);
        }
        Ok(None)
    }

    /// Read `len` bytes of RX buffer `slot` until two reads agree. `None`
    /// if no two of `DMA_TRIES` reads agreed.
    fn rx_payload(&self, slot: u32, len: u32) -> Result<Option<Vec<u8>>, String> {
        let mut reads: Vec<Vec<u8>> = Vec::new();
        for _ in 0..DMA_TRIES {
            let f = self
                .rx_bufs
                .read(slot * BUF_SIZE, len)
                .map_err(|e| dma_err(&format!("read RX buffer {slot} ({len} bytes)"), e))?;
            if f.len() != len as usize {
                return Err(format!(
                    "read RX buffer {slot}: expected {len} bytes, got {}",
                    f.len()
                ));
            }
            if reads.iter().any(|r| *r == f) {
                return Ok(Some(f));
            }
            reads.push(f);
        }
        Ok(None)
    }

    /// Give descriptor `slot` back to the device: clear what it wrote, then
    /// move the tail to it (7.1.8).
    fn recycle(&mut self, slot: u32) -> Result<(), String> {
        self.rx_desc
            .write(slot * DESC_SIZE + 8, &[0u8; 8])
            .map_err(|e| dma_err(&format!("clear RX descriptor {slot}"), e))?;
        write(reg::RDT, "RDT", slot)?;
        self.rx_next = (slot + 1) % RING;
        Ok(())
    }

    /// SPEC.md "`receive`".
    fn receive(&mut self) -> Result<Option<Vec<u8>>, String> {
        let slot = self.rx_next;
        // Cheap first look: a descriptor that does not look done is not
        // taken, whatever the reason.
        let first = self
            .rx_desc
            .read(slot * DESC_SIZE + 12, 1)
            .map_err(|e| dma_err(&format!("read RX descriptor {slot} status"), e))?;
        if first.first().copied().unwrap_or(0) & bit::DESC_DD == 0 {
            if since(self.rx_checked_ns) >= RX_CHECK_NS {
                self.rx_checked_ns = now_ns();
                self.check_rx()?;
            }
            return Ok(None);
        }

        // It looks done: confirm with two agreeing reads.
        let Some(d) = self.rx_descriptor(slot)? else {
            warn(
                "RX descriptor reads disagree; not taken this call",
                &[
                    ("descriptor", slot.to_string()),
                    ("expected", "two identical reads with the buffer address the driver wrote".into()),
                ],
            );
            return Ok(None);
        };
        let status = d[12];
        if status & bit::DESC_DD == 0 {
            return Ok(None);
        }
        // The device writes a descriptor back before advancing RDH past it
        // (7.1.8), so a done descriptor must lie behind the head.
        let rdh = read_index(reg::RDH, "RDH")?;
        if rdh == slot {
            warn(
                "RX descriptor reads done but RDH has not passed it; not taken",
                &[
                    ("descriptor", slot.to_string()),
                    ("status", format!("0x{status:02x}")),
                    ("rdh", rdh.to_string()),
                    ("expected", "RDH past a written-back descriptor (7.1.8)".into()),
                ],
            );
            return Ok(None);
        }
        self.rx_disagree_since = None;

        let len = usize::from(u16::from_le_bytes([d[8], d[9]]));
        let errors = d[13];
        let frame = if status & bit::DESC_EOP == 0
            || errors != 0
            || !(MIN_FRAME..=MAX_FRAME).contains(&len)
        {
            warn(
                "dropped received frame",
                &[
                    ("descriptor", slot.to_string()),
                    ("status", format!("0x{status:02x}")),
                    ("errors", format!("0x{errors:02x}")),
                    ("length", len.to_string()),
                    ("expected", "DD and EOP set, no errors, length 14-1514".into()),
                ],
            );
            None
        } else {
            match self.rx_payload(slot, len as u32)? {
                Some(f) => Some(f),
                None => {
                    warn(
                        "dropped received frame: its buffer read differently every time",
                        &[
                            ("descriptor", slot.to_string()),
                            ("length", len.to_string()),
                            ("expected", format!("two identical reads of {DMA_TRIES}")),
                        ],
                    );
                    None
                }
            }
        };
        self.recycle(slot)?;
        Ok(frame)
    }

    /// Compare the driver's RX position with the device's (SPEC.md
    /// "Noticing a stall"). The driver keeps RDT one behind `rx_next`; the
    /// device has filled the descriptors from `rx_next` up to RDH.
    fn check_rx(&mut self) -> Result<(), String> {
        let rdh = read_index(reg::RDH, "RDH")?;
        let rdt = read_index(reg::RDT, "RDT")?;
        let want_rdt = (self.rx_next + RING - 1) % RING;
        if rdt != want_rdt {
            // Only the driver writes RDT: the device lost its state.
            return self.reinit(&format!(
                "RDT reads {rdt}, but the driver last wrote {want_rdt}"
            ));
        }
        if rdh == self.rx_next {
            self.rx_disagree_since = None;
            return Ok(());
        }
        // RDH says descriptor rx_next is written back. Does it say so?
        if let Some(d) = self.rx_descriptor(self.rx_next)? {
            if d[12] & bit::DESC_DD != 0 {
                self.rx_disagree_since = None;
                return Ok(());
            }
        }
        let Some(t) = self.rx_disagree_since else {
            self.rx_disagree_since = Some(now_ns());
            return Ok(());
        };
        if since(t) < RX_RESYNC_NS {
            return Ok(());
        }
        // Lasting disagreement: the descriptors from rx_next up to RDH are
        // the driver's (7.1.8: the hardware owns head..tail), so drop them,
        // and continue where the device is.
        let skipped = ahead(self.rx_next, rdh);
        warn(
            "receive out of step with the device; resynchronising at RDH",
            &[
                ("rx_next", self.rx_next.to_string()),
                ("rdh", rdh.to_string()),
                ("rdt", rdt.to_string()),
                ("skipped", skipped.to_string()),
                (
                    "expected",
                    "descriptor rx_next written back (DD) whenever RDH has passed it (7.1.8)".into(),
                ),
            ],
        );
        let mut s = self.rx_next;
        while s != rdh {
            self.rx_desc
                .write(s * DESC_SIZE + 8, &[0u8; 8])
                .map_err(|e| dma_err(&format!("clear RX descriptor {s}"), e))?;
            s = (s + 1) % RING;
        }
        self.rx_next = rdh;
        write(reg::RDT, "RDT", (rdh + RING - 1) % RING)?;
        self.rx_disagree_since = None;
        Ok(())
    }

    /// Check that the device still holds the configuration the driver
    /// wrote; re-initialise it if a register reads (twice) otherwise.
    fn check_health(&mut self) -> Result<(), String> {
        let rx_iova = self.rx_desc.iova();
        let tx_iova = self.tx_desc.iova();
        let checks: [(u32, &str, u32, u32); 8] = [
            (reg::RCTL, "RCTL", RCTL_VALUE, RCTL_VALUE),
            (reg::TCTL, "TCTL", bit::TCTL_EN, bit::TCTL_EN),
            (reg::RDBAL, "RDBAL", u32::MAX, rx_iova as u32),
            (reg::RDBAH, "RDBAH", u32::MAX, (rx_iova >> 32) as u32),
            (reg::RDLEN, "RDLEN", u32::MAX, RING_BYTES),
            (reg::TDBAL, "TDBAL", u32::MAX, tx_iova as u32),
            (reg::TDBAH, "TDBAH", u32::MAX, (tx_iova >> 32) as u32),
            (reg::TDLEN, "TDLEN", u32::MAX, RING_BYTES),
        ];
        for (offset, name, mask, want) in checks {
            let a = read(offset, name)?;
            if a & mask == want {
                continue;
            }
            let b = read(offset, name)?;
            if b & mask == want {
                continue;
            }
            return self.reinit(&format!(
                "{name} reads 0x{a:08x} then 0x{b:08x}; expected 0x{want:08x} (mask 0x{mask:08x})"
            ));
        }
        self.check_rx()
    }

    /// Move `tx_clean` up to TDH: TDH counts the descriptors already loaded
    /// into the transmit FIFO (7.2.4), so those before it are free.
    fn reclaim(&mut self) -> Result<(), String> {
        let tdh = read_index(reg::TDH, "TDH")?;
        let pending = ahead(self.tx_clean, self.tx_tail);
        let done = ahead(self.tx_clean, tdh);
        if done <= pending {
            self.tx_clean = tdh;
            self.tx_disagree_since = None;
            return Ok(());
        }
        // TDH outside [tx_clean, tx_tail]: not where the device can be.
        let t = *self.tx_disagree_since.get_or_insert(now_ns());
        warn(
            "TDH outside the descriptors the driver handed over",
            &[
                ("tdh", tdh.to_string()),
                ("tx_clean", self.tx_clean.to_string()),
                ("tx_tail", self.tx_tail.to_string()),
                ("expected", "tx_clean <= TDH <= tx_tail (around the ring)".into()),
            ],
        );
        if since(t) >= TX_RESYNC_NS {
            self.reinit(&format!(
                "TDH reads {tdh}, outside the handed-over descriptors {}..{} for over {} ms",
                self.tx_clean,
                self.tx_tail,
                TX_RESYNC_NS / 1_000_000
            ))?;
        }
        Ok(())
    }

    /// SPEC.md "`transmit`".
    fn transmit(&mut self, frame: Vec<u8>) -> Result<(), String> {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) {
            return Err(format!(
                "frame is {} bytes; expected 14 to 1514 (no FCS)",
                frame.len()
            ));
        }
        let mut data = frame;
        if data.len() < MIN_TX_LEN {
            data.resize(MIN_TX_LEN, 0);
        }

        if ahead(self.tx_clean, self.tx_tail) >= RING / 2 {
            self.reclaim()?;
        }
        let slot = self.tx_tail;
        let next = (slot + 1) % RING;
        if next == self.tx_clean {
            return Err(self.ring_full()?);
        }
        self.tx_full_since = None;

        self.tx_bufs
            .write(slot * BUF_SIZE, &data)
            .map_err(|e| dma_err(&format!("write TX buffer {slot}"), e))?;
        let addr = self.tx_bufs.iova() + u64::from(slot * BUF_SIZE);
        let mut d = [0u8; DESC_SIZE as usize];
        d[0..8].copy_from_slice(&addr.to_le_bytes());
        d[8..10].copy_from_slice(&(data.len() as u16).to_le_bytes());
        d[11] = bit::TXCMD_EOP | bit::TXCMD_IFCS | bit::TXCMD_RS;
        self.tx_desc
            .write(slot * DESC_SIZE, &d)
            .map_err(|e| dma_err(&format!("write TX descriptor {slot}"), e))?;
        write(reg::TDT, "TDT", next)?;
        self.tx_tail = next;
        Ok(())
    }

    /// The ring is full: rewrite TDT (in case the device missed it), watch
    /// for a stalled transmitter, and say why the frame was not queued.
    fn ring_full(&mut self) -> Result<String, String> {
        write(reg::TDT, "TDT", self.tx_tail)?;
        let tdh = read_index(reg::TDH, "TDH")?;
        let link = read(reg::STATUS, "STATUS")? & bit::STATUS_LU != 0;
        let now = now_ns();
        match self.tx_full_since {
            Some((t, h)) if h == tdh => {
                if link && now.saturating_sub(t) >= TX_STALL_NS {
                    self.reinit(&format!(
                        "transmit ring full with TDH stuck at {tdh} for over {} ms while the link is up",
                        TX_STALL_NS / 1_000_000
                    ))?;
                    return Ok(format!(
                        "transmit ring full and the transmitter stalled (TDH stuck at {tdh}); the device was re-initialised, frame not sent"
                    ));
                }
            }
            _ => self.tx_full_since = Some((now, tdh)),
        }
        Ok(format!(
            "transmit ring full: expected a free descriptor, got TDH = {tdh}, TDT = {}, oldest pending descriptor {}, link {}",
            self.tx_tail,
            self.tx_clean,
            if link { "up" } else { "down" }
        ))
    }
}

struct Driver;

impl Guest for Driver {
    fn start() -> Result<(), String> {
        let previous = NIC.with(|n| n.borrow_mut().take());
        let nic = start(previous)?;
        NIC.with(|n| *n.borrow_mut() = Some(nic));
        Ok(())
    }
}

impl ethernet::Guest for Driver {
    fn mac_address() -> (u8, u8, u8, u8, u8, u8) {
        let m = NIC.with(|n| n.borrow().as_ref().map(|nic| nic.mac)).unwrap_or([0; 6]);
        (m[0], m[1], m[2], m[3], m[4], m[5])
    }

    /// STATUS.LU, read until it is not all-ones; a device that does not
    /// answer has no link. Also the driver's periodic health check.
    fn link_up() -> bool {
        let up = match read(reg::STATUS, "STATUS") {
            Ok(s) => s & bit::STATUS_LU != 0,
            Err(e) => {
                warn("link-up: reporting the link down", &[("error", e)]);
                return false;
            }
        };
        NIC.with(|n| {
            if let Some(nic) = n.borrow_mut().as_mut() {
                if since(nic.health_ns) >= HEALTH_NS {
                    nic.health_ns = now_ns();
                    if let Err(e) = nic.check_health() {
                        warn("health check failed", &[("error", e)]);
                    }
                }
            }
        });
        up
    }

    fn transmit(frame: Vec<u8>) -> Result<(), String> {
        with_nic("transmit", |nic| nic.transmit(frame))
    }

    fn receive() -> Result<Option<Vec<u8>>, String> {
        with_nic("receive", |nic| nic.receive())
    }
}

export!(Driver);
