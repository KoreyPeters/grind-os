<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# RTL8139 driver specification

Target: QEMU's emulated `rtl8139` (PCI `10ec:8139`, revision 0x20), which
models the Realtek RTL8139C(L)+. World `nic` (`portals/wit/portals.wit`).
Polling only; no interrupts.

Every fact below comes from **RTL8139C(L)+ datasheet, Rev. 1.6,
2004-09-03, Track ID JATR-1076-21** ("DS"), cited by section, unless it
says otherwise. "Portal" facts come from `portals/wit/portals.wit` and
`portals/README.md`; "Design" facts from
`docs/design/milestone-6-rtl8139.md`; "Shim" facts from `shim/README.md`.
Facts marked **(assumed)** are not stated by the datasheet; each is a
choice the grind can confirm or refute.

## Mode: C+ mode

- DS 2 / 1: the chip has two buffer-management modes. C mode (default) uses
  one contiguous RX ring (RBSTART, 6 "Register Descriptions") and four TX
  slots (TSAD0-3), all **32-bit** addresses. C+ mode, entered by software,
  uses descriptor rings with **64-bit** buffer and ring addresses
  (TNPDS, THPDS, RDSAR; DS 6, 6.34, 9.2).
- Portal + observed in `drivers/e1000e/PROVENANCE.md` (the shim, not the
  chip): DMA IOVAs start at 4 GiB (0x1_0000_0000). The shim README says so
  too ("IOVAs from 4 GiB up"). C mode's 32-bit addresses cannot reach
  them, so **this driver uses C+ mode**.
- DS 6.33: C+CR "is the key to entering C+ mode". Recommended order:
  enable C+ functions in C+CR first, then enable TX/RX in CR (0x37), then
  configure the other registers (descriptor start addresses, TCR, RCR).
- DS 6.33 / 9.13.4 / 8.1.5: C+CR.DAC enables 64-bit (dual address cycle)
  DMA, used only when the high 32 bits are non-zero. Our addresses are
  above 4 GiB, so DAC is set.

## Register window

Design / Portal: registers are in BAR1 (memory, 256 bytes), reached through
`mmio`. Shim: writes touching 0x50 (9346CR), 0x51, 0x52, 0x59, 0x5A
(CONFIG0/1/3/4), 0xD4-0xD7 (FLASH) or 0xD8 (Config5) are denied and fail
the run. The driver never writes those bytes; in particular TPPoll (0xD9)
is written with an 8-bit access only, so it never touches Config5.

## Registers used (DS 6, register table; detail sections as cited)

| Offset | Name | Width | Use | Source |
| --- | --- | --- | --- | --- |
| 0x00-0x05 | IDR0-5 | read 8 | station MAC address, auto-loaded from EEPROM; writes must be 32-bit (not written) | DS 6 |
| 0x08-0x0F | MAR0-7 | write 32 | multicast filter; driver must initialise; 32-bit writes only. Written 0 | DS 6 |
| 0x20-0x27 | TNPDS | write 32 ×2 | normal-priority TX ring start, 64-bit, 256-byte aligned (low dword at 0x20, high at 0x24 **(assumed, by analogy with RDSAR)**) | DS 6 |
| 0x28-0x2F | THPDS | write 32 ×2 | high-priority TX ring start; pointed at an all-zero (host-owned) ring, never polled | DS 6 |
| 0x37 | CR | 8 | RST (bit 4), RE (bit 3), TE (bit 2), BUFE (bit 0, RO) | DS 6.5 |
| 0x3C | IMR | 16 | interrupt mask; written 0 (polling) | DS 6.6 |
| 0x3E | ISR | 16 | status; write 1 to clear. Read for diagnostics | DS 6.7 |
| 0x40 | TCR | 32 | TX config; HWVERID read-only in bits 30-26 and 23 | DS 6.8 |
| 0x44 | RCR | 32 | RX config | DS 6.9 |
| 0x4C | MPC | 32 | missed packet counter, diagnostics only | DS 6 |
| 0x58 | MSR | 8 | bit 2 LINKB: 0 = link OK; bit 3 SPEED_10 | DS 6.13 |
| 0x62 | BMCR | 16 | read for diagnostics only | DS 6.19 |
| 0x64 | BMSR | 16 | bit 2 link status, bit 5 auto-negotiation complete; diagnostics | DS 6.20 |
| 0xD9 | TPPoll | 8 | bit 6 NPQ: write 1 to poll the normal-priority TX queue | DS 6.32 |
| 0xE0 | C+CR | 16 (word only) | bit 0 C+Tx, bit 1 C+Rx, bit 3 MulRW, bit 4 DAC, bit 5 RxChkSum, bit 6 RxVLAN | DS 6.33 |
| 0xE4-0xEB | RDSAR | write 32 ×2 | RX ring start, low dword at 0xE4, high at 0xE8, 256-byte aligned | DS 6.34 |

### Chip identification (DS 6.8)

TCR bits 30-26 and bit 23 identify the chip: RTL8139C+ is 1,1,1,0,1 with
bit 23 = 1. The driver logs the value and refuses to start on anything
else (the C+ descriptor format would not exist).

### Values written

- **CR = 0x10** (RST) to reset; RST reads 1 during reset and clears itself
  when done (DS 6.5). Reset keeps IDR and MAR. Timeout **(assumed)**:
  100 ms.
- **C+CR = 0x0013**: C+Tx, C+Rx, DAC (DS 6.33). MulRW, checksum offload and
  VLAN de-tagging stay off. Written with `write16` (word access only).
- **CR = 0x0C**: RE | TE (DS 6.5).
- **TCR = 0x0300_0700** (DS 6.8): IFG[2:0] = 011 (the only 802.3-compliant
  gap: IFG2 = bit 19 = 0, IFG1:0 = bits 25:24 = 11), MXDMA = 111 (2048-byte
  bursts), CRC bit 16 = 0 (append CRC), loopback 00, TXRR = 0. DS 6.8:
  TCR may only be changed after TE is set, so it is written after CR.
- **RCR = 0x0000_E40A** (DS 6.9): RXFTH = 111 (no threshold: whole packet
  first), MXDMA = 100 (256 bytes; only 010, 011, 100 are defined), AB
  (bit 3, broadcast: ARP requests are broadcast) and APM (bit 1, frames to
  our MAC). AAP, AM, AR, AER off. RBLEN and WRAP are C mode only.
- **MAR0-7 = 0** (no multicast).
- **IMR = 0**; **ISR = 0xFFFF** to clear anything pending (DS 6.6, 6.7).

## Descriptors (DS 9.2)

Each descriptor is 16 bytes (4 dwords, little-endian **(assumed: the
datasheet never states byte order; PCI is little-endian)**). A ring holds
up to 64 consecutive descriptors and starts on a 256-byte boundary; the
last descriptor has EOR set and the device wraps to the first. There is no
ring-length register: EOR is the only end marker. Every buffer is at least
4 bytes.

Rings: **32 RX** and **32 TX** descriptors, each ring in its own 4096-byte
DMA buffer (page-aligned, so 256-byte aligned). Buffers: 2048 bytes each.

### TX descriptor (DS 9.2.1, "Normal" format, LGSEN = 0)

| Dword | Bits | Field |
| --- | --- | --- |
| 0 | 31 | OWN: 1 = owned by the NIC; NIC clears it when transmitted |
| 0 | 30 | EOR |
| 0 | 29, 28 | FS, LS (one descriptor per frame: both set) |
| 0 | 27 | LGSEN = 0 |
| 0 | 18-16 | IPCS, UDPCS, TCPCS = 0 (no offload) |
| 0 | 15-0 | Frame_Length |
| 1 | 17 | TAGC = 0 (no VLAN tag) |
| 2 | 31-0 | TX buffer address low |
| 3 | 31-0 | TX buffer address high |

After transmit (status form): OWN = 0, bit 23 TES (error summary when LS),
bit 22 OWC, 21 LNKF, 20 EXC, 25 UNF, 19-16 collision count.

Transmit: copy the frame into the slot's buffer; write dwords 1-3, then
dword 0 with OWN | FS | LS | (EOR on the last slot) | length; then write
TPPoll = 0x40 (NPQ). Reclaim a slot when its OWN reads 0; log TES and its
causes if set.

**Padding.** DS 9.2 says the chip pads frames under 64 bytes (with CRC)
only "when the PAD bit is set to 1 in the C+CR", but C+CR (DS 6.33) has
no PAD bit. Portal: the host does not pad short frames (an ARP reply is
42 bytes). So the **driver pads** every frame shorter than 60 bytes with
zeros to 60 bytes (60 + 4-byte CRC = 64).

### RX descriptor (DS 9.2.2)

Command form (driver → NIC): dword 0 bit 31 OWN = 1, bit 30 EOR, bits
12-0 Buffer_Size (max 8K; data beyond 4K purged); dword 1 = 0; dwords 2-3
buffer address.

Status form (NIC → driver, OWN = 0): bit 29 FS, 28 LS, 27 FAE, 26 MAR,
25 PAM, 24 BAR, 23 BOVF, 22 FOVF, 21 RWT, 20 RES (error summary, valid
with LS), 19 RUNT, 18 CRC, 17-16 PID, 15 IPF, 14 UDPF, 13 TCPF, 12-0
Frame_Length **including the 4-byte CRC** when LS = 1.

Receive: if the descriptor at `rx_next` has OWN = 1, nothing is waiting.
Otherwise, a frame is accepted when FS and LS are both set, RES is clear
and Frame_Length - 4 is 14-1514; the frame is the first Frame_Length - 4
bytes of the buffer (FCS dropped; Portal: frames exclude the FCS). Any
other descriptor is logged (status dword, expected values) and dropped.
Either way the descriptor is handed back: dword 1 = 0, then dword 0 =
OWN | (EOR on last) | 2048, and `rx_next` advances.

## `start` sequence

1. Probe: read TCR; all-ones on every one of 4 reads means the device is
   not responding. Check HWVERID is C+ (DS 6.8).
2. Reset: CR = RST; poll CR until RST = 0 (DS 6.5). A CR read of all-ones
   does not count as "RST clear".
3. Read MAC from IDR0-5 (DS 6); reject zero or multicast addresses.
4. Allocate the five DMA buffers (RX ring, RX buffers, TX ring, TX
   buffers, high-priority TX ring). These are the only allocations the
   driver ever makes; if one fails, `start` returns the portal's error
   naming the buffer, and the buffers already allocated are dropped.
5. `program` (also used to rebuild the device, see "Robustness
   contract"): IMR = 0, ISR = 0xFFFF (DS 6.6, 6.7); fill the rings: RX
   descriptors OWN with buffers; TX descriptors all zero except EOR on the
   last (host-owned); the all-zero high-priority ring; ring indexes to 0.
6. C+CR = C+Tx | C+Rx | DAC (DS 6.33).
7. CR = RE | TE (DS 6.5, 6.33 order).
8. TNPDS, THPDS, RDSAR (DS 6, 6.34); TCR, RCR (DS 6.8, 6.9); MAR = 0.
   Read CR, C+CR and RCR back.
9. Wait (up to 5 s, **assumed**) for MSR.LINKB = 0 (DS 6.13). Log MSR,
   BMSR, BMCR, CR, C+CR, TCR, RCR.

`link-up` returns MSR.LINKB == 0. `mac-address` returns the IDR value read
in step 3.

## Datasheet gaps and doubts (recorded for the grind)

- PAD bit referenced in 9.2 does not exist in 6.33 (driver pads).
- TNPDS/THPDS are listed as 64-bit at 0x20/0x28 but their dword order is
  not stated; RDSAR's is (6.34). Assumed the same.
- 8.1.4 says the chip "supports only 32-bit data access", while 6 and 6.33
  describe byte registers (CR, TPPoll) and word-only C+CR. The register
  descriptions are followed.
- No explicit reset timeout and no link-up time are given.
- No ring-length register; behaviour on the ring with fewer than 64
  descriptors relies on EOR (9.2).
- The C+ "programming guide" the datasheet refers to (9.1, 9.2) is not
  available; this spec is built without it.

## Robustness contract (`portals/wit` `interface ethernet`, docs/design/robust-nic.md)

Principles, from the contract and the host's fault injection
(`host/src/faults.rs`, read for what the test does, not as a device
fact): a single read of a register or of DMA memory the device writes is
never trusted; one bad value must not leave the driver out of step with
the device; the driver notices when the device has lost its
configuration or stops making progress and rebuilds it itself; every
wait is bounded.

**Reading registers.** Every register the driver decides on (TCR at
probe, CR during reset and health checks, C+CR, RCR, ISR, MSR) is read
with up to 4 tries, and the first value that is not all-ones is taken.
None of these registers is expected to read all-ones in normal
operation: each has reserved bits (CR bits 7-5, DS 6.5; ISR bits 12-9,
DS 6.7; TCR bit 31, DS 6.8; RCR bits 31-28, DS 6.9; MSR bit 5, DS 6.13),
and C+CR bit 8 is "always 0" (DS 6.33). The datasheet does not say what
reserved bits read; that they never all read 1 at once is **assumed**
(observed: CR `0x0d`, ISR `0x0015` at most, TCR `0x77800700`, RCR
`0x0000e40a`, MSR `0xd0`). All-ones on every try means "the device may be gone",
never "every status bit is set". ISR bits are cleared (write 1, DS 6.7)
only from a value that was not all-ones.

**Reading device-written memory.** An RX descriptor (16 bytes), a TX
descriptor's status dword and a received frame's bytes are each read
until two reads agree (at most three). If no two agree, nothing is
consumed and the next call reads again; a frame whose bytes never agree
in 4 calls is dropped, with a warning, and its descriptor handed back.
Only after an agreed read is a descriptor treated as done (OWN = 0,
DS 9.2.1, 9.2.2). An agreed RX descriptor is then validated: the buffer
address (dwords 2-3) must be the one the driver wrote, FS and LS set, RES
clear, Frame_Length - 4 within 14-1514 (DS 9.2.2); anything else is
logged and dropped, and lengths are never used to index before this
check. Re-arming rewrites the whole descriptor (dwords 1-3, then dword 0
with OWN), so a bad value cannot survive in the ring.

**Staying in step with the RX ring.** The device fills descriptors in
ring order from where it stopped (DS 9.2), so when the driver's next
descriptor is still the device's, no other descriptor should be done.
Every 20 ms while idle **(assumed interval)**, the driver reads the whole
ring (agreed reads); if any descriptor is done, the two have fallen out
of step (for example, a bad read once made the driver skip one), and the
driver moves its index to the oldest done descriptor: the first, going
forward from its own index, whose predecessor is not done. Logged as
"RX ring out of step with the device; resynchronised".

**Health check and rebuild.** At most every 200 ms **(assumed)**, at the
start of `receive` and `transmit`, CR, C+CR and RCR are compared with
what was written (RE and TE set, DS 6.5; C+Tx, C+Rx, DAC, DS 6.33; the
RCR value, DS 6.9), and ISR error bits are logged (rate limited) and
cleared. If the device answers but its configuration is lost, or it
answers again after reading all-ones, the driver **rebuilds** it without
the host calling `start`: software reset (DS 6.5: "resets the system
buffer pointer to the initial value", so the device starts again at the
first descriptor) and `program` again with the same DMA buffers. Frames
in the rings are lost. Rebuilds are at most one per 500 ms
**(assumed)**; meanwhile calls return an error saying a rebuild is
pending.

**Each contract item:**

| Condition | How the driver meets it |
| --- | --- |
| Link goes down | `link-up` reads MSR.LINKB (DS 6.13) on every call, so it reports down on the first poll after the device does. `transmit` keeps queueing; if the device holds the descriptors, the ring fills and `transmit` returns "transmit ring full". No call waits for the link. |
| Link comes back | Nothing in the driver depends on the link: rings and configuration are kept, and the device resumes on the next TPPoll (DS 6.32). Observed: frames flow on the first ping after the link returns. A transmit stall (descriptors still the device's for 1.5 s, **assumed**) triggers a rebuild. |
| Receive overflow | Frames the device cannot place are lost by the device (RDU/FOVW, DS 6.7, logged and cleared). The driver keeps handing descriptors back as it consumes them, and the resync above catches any lost step, so reception continues as soon as the host polls again. |
| Hostile frames | Only frames with FS, LS, RES clear and 14-1514 bytes after the FCS are returned (DS 9.2.2); the rest are dropped and their descriptors handed back whole, so later receives are unaffected. Frames to transmit outside 14-1514 bytes are refused with an error. |
| Transmit ring full | `transmit` returns "transmit ring full: oldest pending descriptor N ... ms without progress" immediately, after writing TPPoll.NPQ again (DS 6.32). Never waits. |
| Device reads all-ones | Registers: 4 tries, then all-ones is believed and treated as "device not responding": `receive` and `transmit` return an error saying so, `link-up` returns false, and the reset wait gives up after 100 ms. When the registers answer again, the device is rebuilt. Every wait in the driver has a deadline (reset 100 ms, link at `start` 5 s), and no loop runs more than 32 descriptors. |
| Device writes nonsense into DMA memory | Agreed reads, then validation of status, length and buffer address before use; the frame's bytes are agreed too, so a changed byte is never handed up. Indexes come only from the driver's own counters, never from device memory. |
| DMA allocation fails | `start` returns an error naming the buffer, its size and the portal error; buffers allocated so far are dropped. |
| Over a run | No `unwrap` or indexing on device-supplied values (lengths are checked first; descriptors are read at fixed sizes); no allocation after `start` (a rebuild reuses the buffers); every error string names what was attempted, expected and got. Repeated warnings are rate limited (first 10 of each kind, then one in 500, each with its occurrence count). |
