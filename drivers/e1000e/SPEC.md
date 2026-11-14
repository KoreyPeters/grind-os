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

The host (`grind-host --serve-ping`) calls these one at a time after
`start` and answers ARP and ping itself; the driver only moves frames.

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
Registers are read as in "Robustness contract: reading registers".

1. **Probe.** Read STATUS. `0xFFFFFFFF` (8 times in a row) means the device
   is not responding: fail.
1a. **Allocate** the RX ring, RX buffers, TX ring and TX buffers (see
   "Memory"), all before configuring the device, so a failed allocation
   leaves it untouched. A second `start` reuses the first one's buffers.

Steps 2–15 are also the re-initialisation used for recovery (see
"Robustness contract: recovery").

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
8. **Station address** (10.2.5.22–23): read RAL0 and RAH0, twice; both
   reads must agree (and, on a re-initialisation, equal the address read
   by `start`). Require
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

1. Read the status byte of descriptor `rx_next`. If `DD` = 0: run the RX
   check if 50 ms have passed since the last one ("Noticing a stall"), and
   return `none`.
2. Confirm: read the whole descriptor until two reads agree and its
   address field is the one the driver wrote (7.1.3.1); otherwise return
   `none` this call. `DD` must still be set.
3. Read RDH. If it equals `rx_next`, the device has not written this
   descriptor back (7.1.8): return `none`, do not take it.
4. If `EOP` = 0, or errors ≠ 0, or length is outside 14–1514: log a warning
   naming the descriptor, status, errors and length, recycle the
   descriptor (step 6) and return `none` for this call. With LPE = 0 and
   2048-byte buffers, a valid frame never spans two descriptors.
5. Copy `length` bytes from that descriptor's buffer, reading until two
   reads agree (up to 6); if none agree, drop the frame with a warning.
6. Recycle: zero bytes 8–15 (length, checksum, status, errors, VLAN), then
   RDT ← `rx_next` (this hands the descriptor back), and advance `rx_next`
   modulo 32.
7. Return the frame.

## `transmit`

Keep `tx_tail` (the next free descriptor) and `tx_clean` (the oldest
descriptor not yet confirmed done).

1. Reject frames shorter than 14 or longer than 1514 bytes, naming the
   length. PSP pads short frames to 64 bytes on the wire (10.2.6.1 asks
   for at least 17 bytes when padding; any valid frame is at least 14, so
   a frame of 14–16 bytes is padded in software to 17).
2. Reclaim, when 16 or more descriptors are pending: read TDH (two
   agreeing reads). TDH counts descriptors "already loaded in the transmit
   FIFO" (7.2.4), so if TDH lies between `tx_clean` and `tx_tail`, set
   `tx_clean` ← TDH. Otherwise see "Noticing a stall".
3. If advancing `tx_tail` would make it equal `tx_clean`, the ring is full:
   rewrite TDT (a device that missed the last write sees it again), and
   return an error giving TDH, TDT, `tx_clean` and the link state. If the
   ring stays full with TDH unchanged for 1 s while the link is up,
   re-initialise.
4. Copy the frame into buffer `tx_tail`; write the descriptor: address,
   length, CMD = `EOP` | `IFCS` | `RS`, STA = 0.
5. TDT ← `tx_tail` + 1 modulo 32, then store that as `tx_tail`.

`RS` is set on every descriptor (7.2.10.1.4). The driver does not read the
`DD` it causes: TDH is the authority (step 2), so the driver reads no
device-written TX memory.

## `mac-address`, `link-up`

`mac-address` returns the address read in step 8. `link-up` reads
STATUS.`LU` each time it is called; a STATUS that reads all-ones 8 times
in a row is reported as link down. At most every 500 ms, `link-up` also
runs the health check ("Noticing a stall"); the host calls it every
100 ms.

## Robustness contract

How the driver meets each item of the NIC robustness contract
(`portals/wit` `interface ethernet`, `docs/design/robust-nic.md`). Device
facts are from the datasheet sections cited; "observed" marks behaviour
seen on QEMU 6.2.0's `e1000e` (recorded in `PROVENANCE.md`).

### Principles

1. **No single read is trusted.** Registers and device-written memory are
   both read again before the driver acts on them (below).
2. **The head registers are the authority** on what the device has
   finished. RDH: "Hardware OWNS all descriptors between [head... tail]",
   and used descriptors are written back "just prior to advancing the head
   pointer" (7.1.8). TDH "returns the value of head corresponding to
   descriptors already loaded in the transmit FIFO" (7.2.4).
3. **A disagreement that lasts is resynchronised, not waited on.** The
   driver never waits forever on a descriptor; it compares its own ring
   positions with the device's and moves to where the device is.
4. **Lost configuration is rebuilt by a reset**, reusing the buffers from
   `start`. CTRL.RST is "a last-ditch software mechanism to recover from an
   indeterminate or suspected hung hardware state"; it returns the receive
   and transmit registers to their power-on values (10.2.2.1). RDH and TDH
   may be written only after a reset and before RCTL.EN / TCTL.EN (10.2.5.8,
   10.2.6.7), so every ring rebuild goes through the reset.
5. **Every wait is bounded**, every retry loop has a fixed count, and no
   call sleeps except the bounded waits of the reset sequence (about 110 ms
   at most) and `start`'s 5 s link wait (`start` has 15 s).

### Reading registers

No register the driver reads can hold `0xFFFFFFFF` in this configuration:
STATUS, CTRL, RCTL, TCTL, GCR and the length and index registers have
reserved fields that read 0 (10.2.2.1–2, 10.2.3.1, 10.2.5.1, 10.2.5.7–9,
10.2.6.1, 10.2.6.6–8), RDBAL and TDBAL hold 4096-aligned addresses,
RDBAH and TDBAH the high half of an IOVA the DMA portal gave (observed:
1), and an all-ones RAL0 would be a multicast address, which step 8
rejects. So all-ones is what a removed or wedged PCIe device returns,
never a value. `read` retries such a read up to 8 times and then reports "device not responding: <register> read 0xffffffff 8 times in
a row". Read-modify-writes (CTRL, GCR) use only such a validated read, so
an all-ones read is never written back (it would set CTRL.RST and
CTRL.PHY_RST).

Ring indexes (RDH, RDT, TDH, TDT) are 16-bit fields in a ring of 32
(10.2.5.8–9, 10.2.6.7–8): `read_index` needs two reads that agree and
are below 32, within 12 reads; a read of 32 or more (all-ones) is skipped,
since it says nothing about the index. Otherwise it returns an error
naming every value it got. (Observed under injected faults: all-ones reads
can alternate with good ones, so requiring two *consecutive* good reads
failed needlessly; see the grind log.)

The station address (RAL0/RAH0) is read twice and both reads must agree;
after a re-initialisation it must equal the address read by `start`.

### Reading device-written memory

- **RX descriptor.** A first one-byte read of the status decides only that
  a descriptor is *not* ready (a wrong "not done" only delays a frame).
  Before a descriptor is taken, the whole 16 bytes are read until two
  reads agree (up to 6), and bytes 0–7 must be the buffer address the
  driver wrote: the device writes only length, checksum, status, errors
  and VLAN (7.1.3.1), so a different address is a corrupted read. Then
  RDH must have moved past the descriptor (7.1.8): if RDH equals it, the
  descriptor cannot have been written back and is not taken. Only then is
  the length checked (14–1514), with EOP set and errors 0, or the frame is
  dropped with a warning.
- **RX payload.** Read until two reads agree (up to 6); otherwise the
  frame is dropped with a warning. A frame that changes between reads is
  never handed up.
- **TX.** The driver reads no device-written TX memory: TDH (validated,
  as above) says which descriptors are free (7.2.4).

### Items of the contract

| Contract item | How |
| --- | --- |
| Link goes down: `link-up` false within 1 s | `link-up` reads STATUS.LU (10.2.2.2) on every call; a device that does not answer (8 all-ones reads) is reported as link down. |
| Link comes back: up and frames flow within 5 s, no new `start` | Nothing is torn down while the link is down. QEMU sets STATUS.LU about 0.6 s after the link returns (observed). A transmit ring that filled while the link was down is restarted by rewriting TDT when `transmit` finds it full; a full ring whose TDH has not moved for 1 s while the link is up is re-initialised. |
| Receive overflow | When the ring is full the device stops storing frames until the tail moves (7.1.8). Each `receive` hands one descriptor back (RDT), so draining restarts receive. Every 50 ms an idle `receive` checks RDH and RDT against its position (below). |
| Hostile frames | Length outside 14–1514, EOP clear or any error bit: dropped with a warning, descriptor recycled. Frames are copied out of fixed 2048-byte buffers, so no length can reach outside them. |
| Transmit ring full | `transmit` returns "transmit ring full: expected a free descriptor, got TDH, TDT, oldest pending, link" at once. It never waits. |
| Registers read all-ones | Every read retries a fixed number of times, then returns an error naming the register; `link-up` reports down. Configuration is not "repaired" on all-ones reads: only a valid value that differs (read twice) triggers re-initialisation, so a device that comes back with its configuration intact just continues, and one that comes back reset is re-initialised. |
| Device writes nonsense into DMA memory | Section "Reading device-written memory": two agreeing reads, address check, RDH check, length/status check. A bad value is a dropped frame with a warning. |
| DMA allocation fails | `start` allocates all four buffers before writing any register except a STATUS read, and returns the portal's error naming the buffer. Buffers already allocated are dropped (freed by the host). |
| Over a run: no growth in DMA memory | Only `start` allocates; re-initialisation reuses the same buffers, and a second `start` reuses the first one's. |
| Every error says what was attempted, expected and got | All errors are built from the register or buffer name, the expected value and the value read. Repeated warnings are logged in full 10 times, then every 100th, with a count. |

### Noticing a stall

The driver keeps RDT one behind `rx_next` (`start` step 10 and `receive`
step 6).
The device has filled the descriptors from `rx_next` up to RDH (7.1.8).

- **RX check** (an idle `receive`, at most every 50 ms, and in `link-up`'s
  health check): read RDH and RDT.
  - RDT is not one behind `rx_next`: only the driver writes RDT, so the
    device lost its state: re-initialise.
  - RDH equals `rx_next`: nothing waiting; consistent.
  - RDH has passed `rx_next`, but descriptor `rx_next` is not done: if
    this lasts 100 ms, the descriptors from `rx_next` up to RDH belong to
    the driver (7.1.8); their status is cleared, `rx_next` moves to RDH and
    RDT to RDH − 1. This is the state the old driver deadlocked in (RDT set
    to a descriptor the device had not filled: head equal to tail, an
    empty ring); it is now recognised and left within about 100 ms.
- **Health check** (`link-up`, at most every 500 ms; the host polls it
  every 100 ms): RCTL, TCTL.EN, RDBAL/H, RDLEN, TDBAL/H, TDLEN must hold
  what the driver wrote. A differing value read twice means the device was
  reset or lost its configuration: re-initialise. Then the RX check.
- **TX check** (when at least 16 descriptors are pending, and when the
  ring is full): TDH must lie between the oldest pending descriptor and
  the tail. Outside it for 100 ms: re-initialise.

### Recovery (re-initialisation)

Steps 2–15 of the `start` sequence, unchanged, on the existing buffers:
reset, rewrite every descriptor, RDH = TDH = 0, RDT = 31, TDT = 0. It does
not wait for the link (the device brings it back on its own, 4.6.3.2;
`link-up` reports it), so it finishes inside one call. Frames in the rings
are lost. A failed re-initialisation is reported and tried again at the
next health check.

### How the recovery paths were checked

With the validation above, the test's faults never reach the resync or
re-initialisation paths, so each was run once on purpose, through the
harness, with a temporary driver change that was then removed (grind log
in `PROVENANCE.md`, runs 5 and 6):

- with `receive` trusting a single descriptor read again (the old
  behaviour), the driver fell into the old deadlock (RDH = RDT, one behind
  `rx_next`) five times; each time the RX check resynchronised it, and
  the run passed;
- with one re-initialisation forced 20 s into serving (just after the
  third link flap), traffic continued and the run passed, with no DMA
  growth.

## Open questions

- The real target (OptiPlex 7050 Micro) has an i219, a different MAC/PHY
  with its own datasheet. This spec covers only the 82574L that QEMU
  emulates.
- PHY setup through MDIC is skipped because QEMU's PHY auto-negotiates on
  its own. Real hardware may need it.
- Ordering between DMA writes and the tail-register write depends on the
  host. The portals are called one at a time; the shim must make DMA
  writes visible before the MMIO write that follows them.
