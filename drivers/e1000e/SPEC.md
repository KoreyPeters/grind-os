<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# e1000e driver spec (Intel 82574L, as emulated by QEMU `e1000e`)

Written from the Intel 82574 GbE Controller Family Datasheet rev 2.5
(see `PROVENANCE.md`). Section numbers below refer to that datasheet. No
Linux driver code was consulted.

## Scope

Milestone 4: bring the link up, then move Ethernet frames between the host
and the wire so that the host can answer ARP and ping. Polling only. Out of
scope: interrupts, flow control, VLANs, checksum and segmentation offload,
jumbo frames, multiple queues, power management, PHY register access
(MDIC), statistics, and NVM (never written; rule 10).

## Interface

World `nic` (`portals/wit`): exports `start` and the `ethernet` interface
(`mac-address`, `link-up`, `transmit`, `receive`). Frames exclude the FCS.

> The `ethernet` interface is a proposal awaiting the human's approval. If
> it changes, this section changes with it.

## Registers used (BAR0 offsets)

| Register | Offset | Section | Use |
| --- | --- | --- | --- |
| CTRL | 0x00000 | 10.2.2.1 | reset, master disable, link setup |
| STATUS | 0x00008 | 10.2.2.2 | link up, master enable status |
| FCAL, FCAH, FCT | 0x00028, 0x0002C, 0x00030 | 10.2.2.8–10 | written 0 (no XOFF flow control, 4.6.2) |
| ICR | 0x000C0 | 10.2.4.1 | read to clear pending causes |
| IMC | 0x000D8 | 10.2.4.6 | mask all interrupts |
| RCTL | 0x00100 | 10.2.5.1 | receive control |
| TCTL | 0x00400 | 10.2.6.1 | transmit control |
| TIPG | 0x00410 | 10.2.6.2 | inter-packet gap |
| RDBAL, RDBAH, RDLEN, RDH, RDT (queue 0) | 0x02800–0x02818 | 10.2.5.5–9 | RX ring |
| TDBAL, TDBAH, TDLEN, TDH, TDT (queue 0) | 0x03800–0x03818 | 10.2.6.4–8 | TX ring |
| TXDCTL (queue 0) | 0x03828 | 10.2.6.11 | TX write-back policy |
| MTA[0..127] | 0x05200–0x053FC | 10.2.5.21 | zeroed |
| RAL0, RAH0 | 0x05400, 0x05404 | 10.2.5.22–23 | station address (read only) |
| GCR | 0x05B00 | 10.2.3.1 | bit 22 set during init |

None of these are on the shim's deny list. EEC, EERD, EEWR, FLA and the
other NVM/flash registers are never touched, not even read.

### Bits

- CTRL: `GIO_MASTER_DISABLE` bit 2; bit 3 reserved, "set to 1b"; `ASDE`
  bit 5 (must be 0 on the 82574); `SLU` bit 6; `FRCSPD` bit 11;
  `FRCDPLX` bit 12; `RST` bit 26 (self-clearing); `RFCE` 27, `TFCE` 28;
  `VME` 30; `PHY_RST` 31.
- STATUS: `LU` bit 1; `GIO_MASTER_ENABLE_STATUS` bit 19.
- RCTL: `EN` 1, `UPE` 3, `MPE` 4, `LPE` 5, `LBM` 7:6, `DTYP` 11:10,
  `BAM` 15, `BSIZE` 17:16, `VFE` 18, `BSEX` 25, `SECRC` 26, `FLXBUF` 30:27.
- TCTL: `EN` 1, `PSP` 3, `CT` 11:4, `COLD` 21:12.
- TXDCTL: `WTHRESH` 21:16, `GRAN` 24.
- RAH: `AV` bit 31.

## Memory (DMA portal)

- RX ring: 32 legacy descriptors × 16 bytes = 512 bytes (RDLEN must be a
  multiple of 128, base 16-byte aligned: a 4096-byte buffer satisfies both).
- TX ring: the same, 32 × 16 bytes.
- RX buffers: 32 × 2048 bytes (RCTL.BSIZE = 00b with BSEX = 0, so 2048).
  Allocated as one 64 KiB DMA buffer; descriptor *i* points at offset
  2048 × *i*.
- TX buffers: 32 × 2048 bytes, laid out the same way.

IOVAs come from the DMA portal and start at 4 GiB, so the high halves
(RDBAH, TDBAH, and the upper 32 bits of each descriptor address) are
non-zero and must be programmed.

### Legacy RX descriptor (7.1.3)

| Bytes | Field |
| --- | --- |
| 0–7 | buffer address (little-endian) |
| 8–9 | length (bytes written, excludes CRC when SECRC = 1) |
| 10–11 | packet checksum (ignored) |
| 12 | status: `DD` bit 0, `EOP` bit 1 |
| 13 | errors: `CE` 0, `SE` 1, `SEQ` 2, `CXE` 4, `TCPE` 5, `IPE` 6, `RXE` 7 |
| 14–15 | VLAN tag (ignored) |

### Legacy TX descriptor (7.2.10.1)

| Bytes | Field |
| --- | --- |
| 0–7 | buffer address |
| 8–9 | length |
| 10 | CSO (0) |
| 11 | CMD: `EOP` 0, `IFCS` 1, `IC` 2, `RS` 3, `DEXT` 5 (0 = legacy), `VLE` 6, `IDE` 7 |
| 12 | STA: `DD` bit 0 (low nibble) |
| 13 | CSS (0) |
| 14–15 | VLAN (0) |

## `start` sequence

Each step that waits has a bounded timeout. Every failure returns an error
that names the step, the register, the expected value and the value read.

1. **Probe.** Read STATUS. `0xFFFFFFFF` means the device is not responding:
   fail.
2. **Mask interrupts** (4.6.1): IMC ← `0xFFFFFFFF`.
3. **Quiesce** (CTRL note in 10.2.2.1, which points to 3.1.3.10): RCTL ← 0,
   TCTL ← 0; set CTRL.`GIO_MASTER_DISABLE`; poll STATUS bit 19 until clear
   (timeout 10 ms).
4. **Global reset** (4.6.2, 10.2.2.1): CTRL ← CTRL | `RST`. Wait, then
   poll until `RST` reads 0 (timeout 100 ms). The datasheet says to wait
   "approximately 1 µs" before the first access. In the PDF that unit is a
   Symbol-font glyph that text extraction drops, so it reads as "1 s"; the
   driver sleeps 1 ms, which covers both readings of "µ".
5. **Mask interrupts again** (4.6.1): IMC ← `0xFFFFFFFF`; read ICR to clear
   anything pending.
6. **General configuration** (4.6.2): GCR bit 22 ← 1. FCAL, FCAH, FCT ← 0.
7. **Link setup** (4.6.3.2, the preferred mechanism, where the MAC follows
   the PHY): in CTRL, set `SLU` and bit 3; clear `ASDE`, `FRCSPD`,
   `FRCDPLX`, `RFCE`, `TFCE`, `VME`, `PHY_RST`, `GIO_MASTER_DISABLE`.
8. **Station address** (10.2.5.22–23): read RAL0 and RAH0. Require
   RAH0.`AV` = 1, or fail ("no valid station address loaded from NVM"). The
   MAC is RAL0 bytes 0–3 (little-endian) then RAH0 bytes 0–1. Reject an
   all-zero, broadcast or multicast (bit 0 of byte 0) address.
9. **Multicast table** (4.6.5): MTA[0..127] ← 0.
10. **RX ring** (4.6.5, 4.6.5.1): allocate the ring and buffers; write each
    descriptor's buffer address and zero its status. RDBAL/RDBAH ← ring
    IOVA; RDLEN ← 512; RDH ← 0; RDT ← 31. The hardware owns descriptors
    from head up to tail; head == tail means it owns none, so at most 31
    of the 32 descriptors can be handed over.
11. **RCTL** (10.2.5.1) ← `EN` | `BAM` | `SECRC`, with all other fields 0:
    legacy descriptors, 2048-byte buffers, no promiscuous modes, no long
    packets, no VLAN filter, normal (non-loopback) mode.
12. **TX ring** (4.6.6): allocate the ring and buffers. TDBAL/TDBAH ← ring
    IOVA; TDLEN ← 512; TDH ← 0; TDT ← 0.
13. **TXDCTL** (4.6.6) ← `GRAN` = 1, `WTHRESH` = 1, everything else 0.
14. **TCTL** (4.6.6) ← `EN` | `PSP` | `CT` = 0x0F | `COLD` = 0x3F (full
    duplex).
15. **TIPG** ← `0x00602006`. 4.6.6 lists IPGT = 8, IPGR1 = 2, IPGR2 = 10,
    but 10.2.6.2 says the 82574 adds 6 MAC clocks and recommends
    `0x00602006`. The register-level section is the more specific one, so
    its value wins.
16. **Wait for link** (10.2.2.2): poll STATUS.`LU` every 10 ms for up to
    5 s. If the link never comes up, fail and report STATUS.

Statistics (4.6.4) are not cleared: nothing reads them yet.

## `receive`

Keep `rx_next` (the next descriptor the hardware will fill, starting at 0).

1. Read descriptor `rx_next`. If `DD` = 0: return `none`.
2. If `EOP` = 0, or errors ≠ 0, or length is outside 14–1514: log a warning
   naming the descriptor, status, errors and length, recycle the
   descriptor (step 4) and return `none` for this call. With LPE = 0 and
   2048-byte buffers, a valid frame never spans two descriptors.
3. Copy `length` bytes from that descriptor's buffer.
4. Recycle: zero the status byte, then RDT ← `rx_next` (this hands the
   descriptor back), and advance `rx_next` modulo 32.
5. Return the frame.

## `transmit`

Keep `tx_tail` (the next free descriptor) and `tx_clean` (the oldest
descriptor not yet confirmed done).

1. Reject frames shorter than 14 or longer than 1514 bytes, naming the
   length. PSP pads short frames to 64 bytes on the wire (10.2.6.1 asks
   for at least 17 bytes when padding; any valid frame is at least 14, so
   a frame of 14–16 bytes is padded in software to 17).
2. Reclaim: while `tx_clean` ≠ `tx_tail` and descriptor `tx_clean` has
   `DD` set, advance `tx_clean`.
3. If advancing `tx_tail` would make it equal `tx_clean`, the ring is full:
   return an error giving TDH, TDT, `tx_clean` and `tx_tail`.
4. Copy the frame into buffer `tx_tail`; write the descriptor: address,
   length, CMD = `EOP` | `IFCS` | `RS`, STA = 0.
5. TDT ← `tx_tail` + 1 modulo 32, then store that as `tx_tail`.

`RS` on every descriptor makes the hardware write `DD` back for each one
(7.2.10.1.4), which step 2 relies on.

## `mac-address`, `link-up`

`mac-address` returns the address read in step 8. `link-up` reads
STATUS.`LU` each time it is called.

## Open questions

- The real target (OptiPlex 7050 Micro) has an i219, a different MAC/PHY
  with its own datasheet. This spec covers only the 82574L that QEMU
  emulates.
- PHY setup through MDIC is skipped because QEMU's PHY auto-negotiates on
  its own. Real hardware may need it.
- Ordering between DMA writes and the tail-register write depends on the
  host. The portals are called one at a time; the shim must make DMA
  writes visible before the MMIO write that follows them.
