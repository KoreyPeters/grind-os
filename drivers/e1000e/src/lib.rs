// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wasm driver for the Intel 82574L (QEMU `e1000e`), world `nic`.
//!
//! Implements `SPEC.md`, which is written from the Intel 82574 GbE
//! Controller Family Datasheet rev 2.5; section numbers below refer to it.
//! Polling only, legacy descriptors, one RX and one TX queue.

wit_bindgen::generate!({ path: "../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;
use grind::portals::{clock, dma, log, mmio};
use std::cell::RefCell;

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

// Timeouts, in microseconds.
const MASTER_DISABLE_TIMEOUT_US: u64 = 10_000;
const RESET_TIMEOUT_US: u64 = 100_000;
const LINK_TIMEOUT_US: u64 = 5_000_000;

fn read(offset: u32, name: &str) -> Result<u32, String> {
    mmio::read32(offset).map_err(|e| format!("read {name} (0x{offset:05x}) failed: {e:?}"))
}

fn write(offset: u32, name: &str, value: u32) -> Result<(), String> {
    mmio::write32(offset, value)
        .map_err(|e| format!("write {name} (0x{offset:05x}) <- 0x{value:08x} failed: {e:?}"))
}

fn dma_err(what: &str, e: dma::Error) -> String {
    format!("{what}: {e:?}")
}

fn info(message: &str, fields: &[(&str, String)]) {
    let fields: Vec<(String, String)> = fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    log::emit(log::Level::Info, message, &fields);
}

fn warn(message: &str, fields: &[(&str, String)]) {
    let fields: Vec<(String, String)> = fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    log::emit(log::Level::Warn, message, &fields);
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

fn status_now() -> String {
    match mmio::read32(reg::STATUS) {
        Ok(v) => format!("STATUS = 0x{v:08x}"),
        Err(e) => format!("STATUS unreadable: {e:?}"),
    }
}

fn ctrl_now() -> String {
    match mmio::read32(reg::CTRL) {
        Ok(v) => format!("CTRL = 0x{v:08x}"),
        Err(e) => format!("CTRL unreadable: {e:?}"),
    }
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
    /// Oldest TX descriptor not yet seen done.
    tx_clean: u32,
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

/// SPEC.md "`start` sequence".
fn start() -> Result<Nic, String> {
    // 1. Probe.
    let status = read(reg::STATUS, "STATUS")?;
    if status == u32::MAX {
        return Err("STATUS read 0xffffffff: the device is not responding".into());
    }

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

    // 4. Global reset (4.6.2, 10.2.2.1). The datasheet asks for ~1 us before
    //    the next access; 1 ms covers it.
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
    read(reg::ICR, "ICR")?;
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
            | bit::CTRL_GIO_MASTER_DISABLE);
    write(reg::CTRL, "CTRL", ctrl)?;

    // 8. Station address (10.2.5.22-23).
    let ral = read(reg::RAL0, "RAL0")?;
    let rah = read(reg::RAH0, "RAH0")?;
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

    // 9. Multicast table (4.6.5).
    for i in 0..reg::MTA_ENTRIES {
        write(reg::MTA + 4 * i, "MTA", 0)?;
    }

    // 10. RX ring (4.6.5, 4.6.5.1).
    let rx_desc = allocate("RX descriptor ring", RING_BYTES)?;
    let rx_bufs = allocate("RX buffers", RING * BUF_SIZE)?;
    let rx_bufs_iova = rx_bufs.iova();
    let mut ring = vec![0u8; RING_BYTES as usize];
    for i in 0..RING {
        let at = (i * DESC_SIZE) as usize;
        let addr = rx_bufs_iova + u64::from(i * BUF_SIZE);
        ring[at..at + 8].copy_from_slice(&addr.to_le_bytes());
    }
    rx_desc
        .write(0, &ring)
        .map_err(|e| dma_err("write RX descriptors", e))?;
    let rx_iova = rx_desc.iova();
    write(reg::RDBAL, "RDBAL", rx_iova as u32)?;
    write(reg::RDBAH, "RDBAH", (rx_iova >> 32) as u32)?;
    write(reg::RDLEN, "RDLEN", RING_BYTES)?;
    write(reg::RDH, "RDH", 0)?;
    write(reg::RDT, "RDT", RING - 1)?;

    // 11. RCTL (10.2.5.1): legacy descriptors, 2048-byte buffers.
    write(reg::RCTL, "RCTL", bit::RCTL_EN | bit::RCTL_BAM | bit::RCTL_SECRC)?;

    // 12. TX ring (4.6.6).
    let tx_desc = allocate("TX descriptor ring", RING_BYTES)?;
    let tx_bufs = allocate("TX buffers", RING * BUF_SIZE)?;
    let tx_iova = tx_desc.iova();
    write(reg::TDBAL, "TDBAL", tx_iova as u32)?;
    write(reg::TDBAH, "TDBAH", (tx_iova >> 32) as u32)?;
    write(reg::TDLEN, "TDLEN", RING_BYTES)?;
    write(reg::TDH, "TDH", 0)?;
    write(reg::TDT, "TDT", 0)?;

    // 13-15. TXDCTL, TCTL, TIPG (4.6.6, 10.2.6.2).
    write(
        reg::TXDCTL,
        "TXDCTL",
        bit::TXDCTL_GRAN | (1 << bit::TXDCTL_WTHRESH_SHIFT),
    )?;
    write(
        reg::TCTL,
        "TCTL",
        bit::TCTL_EN
            | bit::TCTL_PSP
            | (0x0F << bit::TCTL_CT_SHIFT)
            | (0x3F << bit::TCTL_COLD_SHIFT),
    )?;
    write(reg::TIPG, "TIPG", TIPG_VALUE)?;

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
            ("mac", fmt_mac(&mac)),
            ("status", format!("0x{:08x}", read(reg::STATUS, "STATUS")?)),
            ("waited_us", link_us.to_string()),
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
        tx_tail: 0,
        tx_clean: 0,
    })
}

fn fmt_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

impl Nic {
    /// SPEC.md "`receive`".
    fn receive(&mut self) -> Result<Option<Vec<u8>>, String> {
        let slot = self.rx_next;
        let at = slot * DESC_SIZE;
        let d = self
            .rx_desc
            .read(at, DESC_SIZE)
            .map_err(|e| dma_err(&format!("read RX descriptor {slot}"), e))?;
        let status = d[12];
        if status & bit::DESC_DD == 0 {
            return Ok(None);
        }
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
            Some(
                self.rx_bufs
                    .read(slot * BUF_SIZE, len as u32)
                    .map_err(|e| dma_err(&format!("read RX buffer {slot} ({len} bytes)"), e))?,
            )
        };

        // Recycle: clear length/checksum/status/errors/VLAN, then hand the
        // previous gap descriptor back by moving the tail to this one.
        self.rx_desc
            .write(at + 8, &[0u8; 8])
            .map_err(|e| dma_err(&format!("clear RX descriptor {slot}"), e))?;
        write(reg::RDT, "RDT", slot)?;
        self.rx_next = (slot + 1) % RING;
        Ok(frame)
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

        // Reclaim descriptors the hardware has finished.
        while self.tx_clean != self.tx_tail {
            let sta = self
                .tx_desc
                .read(self.tx_clean * DESC_SIZE + 12, 1)
                .map_err(|e| dma_err(&format!("read TX descriptor {}", self.tx_clean), e))?[0];
            if sta & bit::DESC_DD == 0 {
                break;
            }
            self.tx_clean = (self.tx_clean + 1) % RING;
        }

        let slot = self.tx_tail;
        let next = (slot + 1) % RING;
        if next == self.tx_clean {
            return Err(format!(
                "transmit ring full: TDH = {}, TDT = {}, oldest pending descriptor {}, next free {}",
                read(reg::TDH, "TDH")?,
                read(reg::TDT, "TDT")?,
                self.tx_clean,
                self.tx_tail
            ));
        }

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
        match read(reg::STATUS, "STATUS") {
            Ok(s) => s & bit::STATUS_LU != 0,
            Err(e) => {
                warn("link-up: STATUS unreadable", &[("error", e)]);
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
