<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Provenance: rtl8139 driver

## Sources consulted

- **Realtek RTL8139C(L)+ datasheet, Rev. 1.6, 2004-09-03, Track ID
  JATR-1076-21** (`spec-8139cp(160).pdf`), kept in the gitignored
  `.cache/datasheets/rtl8139/`, sha256
  `f3973ef7d9f6cc9cf0bc372c19323a383d38c6f1f4465063d1be885fc83a38cc`
  (verified before use). Obtained by the human from
  `https://people.freebsd.org/~wpaul/RealTek/` (see
  `docs/design/milestone-6-rtl8139.md`). Read as text with
  `pdftotext -layout`. Used for facts only: sections 1, 6 (register table),
  6.5-6.9, 6.13, 6.19, 6.20, 6.32-6.34, 8.1.4, 8.1.5, 8.3, 9.2, 9.13.4.
  No text copied beyond register and field names.
- **Realtek RTL8139C(L) datasheet, Rev. 1.6, 2005-12-29**
  (`spec-8139c(160).pdf`), same place, sha256
  `9c0a6bd3ae35551f9bd024443776b5b5a7797527c163e6940389821dff47e93b`.
  Only read, to cross-check CR.RST and MSR.LINKB; it has no C+ registers.
- **Project files** (trusted, read only): `CLAUDE.md`,
  `docs/design/milestone-6-rtl8139.md`, `portals/wit/portals.wit`,
  `portals/README.md`, `tests/nic/README.md`, `harness/README.md`,
  `shim/README.md` (deny list; IOVAs from 4 GiB).
- **`drivers/e1000e/`** (this project, AGPL-3.0-or-later): read for the
  crate layout, logging helpers and code style only. No device facts taken;
  the helper pattern (`read`/`write` wrappers, `wait_until`, `info`/`warn`,
  `thread_local` state) is adapted from its `src/lib.rs` into
  `src/lib.rs` (same project and license). Its PROVENANCE note that IOVAs
  start at `0x100000000` was used as a fact about the shim.

## Not consulted

- Any other RTL8139 driver (FreeBSD `rl(4)`/`if_rl.c`, Linux
  `8139too`/`8139cp`, others), QEMU's source (including
  `hw/net/rtl8139.c`), and web searches.

## How they were used

`SPEC.md` was written from the datasheet sections above, citing each fact;
`src/lib.rs` implements `SPEC.md`. No code was copied or translated from
any RTL8139 source.

## Observations of device behaviour

QEMU 6.2.0 `rtl8139` behind a PCIe root port, running this driver
(run `harness/runs/rtl8139/20261114T162939Z`):

- TCR reads `0x74800000` before configuration: HWVERID bits 30-26 =
  11101 and bit 23 = 1, the RTL8139C+ code from DS 6.8.
- CR.RST clears within the first 100 us poll (13 us measured).
- IDR0-5 read 52:54:00:12:34:56, the MAC the VM assigns.
- After configuration: CR = `0x0d` (RE, TE, BUFE), C+CR = `0x0013` (as
  written), TCR = `0x77800700` (the written `0x03000700` plus the
  read-only HWVERID bits), RCR = `0x0000e40a` (as written), MSR = `0xd0`
  (TXFCE, RXFCE, Aux_Status, LINKB = 0: link OK), BMCR = `0x1000` (ANE),
  BMSR = `0x782d` (auto-negotiation complete, link valid), MPC = 0.
- Link is up on the first MSR read after configuration (10 us); no wait
  needed.
- DMA IOVAs are above 4 GiB (RX ring `0x100000000`, TX ring
  `0x100013000`), so the high dwords of RDSAR and TNPDS are 1; C+ mode
  with these 64-bit addresses works. The assumed TNPDS dword order (low at
  0x20, high at 0x24) is consistent with the device transmitting.
- Enabling RE/TE (step 7) before writing RDSAR/TNPDS (step 8), in the
  order DS 6.33 recommends, caused no IOMMU fault in `serial.log`.
- 42-byte ARP replies padded by the driver to 60 bytes were accepted by
  the peer. Whether the device would pad by itself (the missing C+CR PAD
  bit, DS 9.2) was not tested.
- RX frames arrive with FS and LS set and RES clear; Frame_Length minus
  4 gives the frame without FCS (the peer's ARP and echo checks pass on
  the replies built from them).
- No `ISR reports an error condition` and no `dropped received frame`
  entries: the ISR error bits never set during the test.

## Grind log

One entry per `harness/grind.sh rtl8139` run.

### Run 1: `harness/runs/rtl8139/20261114T162939Z`: pass

First run (the native host was built first). All rule checks passed; the
driver built (77,762 bytes of Wasm). `NIC step=arp` passed after 5 ARP
requests (2.1 s, the VM booting), `ping-1`..`ping-3` replied in about
1.2-1.3 ms each, `grind-host` exited 0 with 0 violations and 0 refused
commands; no `mmio-denied`. Nothing to fix. Observations recorded above.

### Run 2: `harness/runs/rtl8139/20261114T163255Z`: pass

Confirmation run; the only change since run 1 was this file. Same
result: ARP after 5 requests (2.1 s), pings 1.3-1.4 ms, 0 violations,
0 refused commands, no driver warnings.

## C-mode experiment (2026-11-14)

After issue #13 added `dma.allocate32`, a second fresh session wrote a
C-mode driver for this chip (receive ring buffer via RBSTART/CAPR/CBR,
transmit through TSD0-3/TSAD0-3, C+ mode left off, all DMA buffers below
4 GiB), from the same two datasheets under the same rules (no other
RTL8139 driver, this directory's earlier driver included; no QEMU source;
no web). It passed `harness/grind.sh rtl8139` on its first run and on a
confirmation run, so again no failure exercised the grind loop. It was
not kept: one driver per device, and the C+ driver covers the test.

Both datasheets defer C-mode buffer details to a Realtek programming
guide that was not available. The session recorded each gap as an
assumption and settled it against QEMU 6.2.0:

- Rx packet header: 4 bytes, a status word (C datasheet 6.1) then the
  length, both little-endian; the length includes the 4-byte CRC and
  excludes the header (seen: `01 20 40 00` for a broadcast ARP, length
  64).
- CAPR holds the read offset minus 16 (its initial value is 0xFFF0);
  writing next offset - 16 empties the ring.
- With RCR.WRAP = 1 and RBLEN = 8K + 16, offsets wrap at 8192, and a
  packet is contiguous past the end. Confirmed only once `tests/nic`
  gained its bulk step (commit 76719a0): the ring wrapped about 45 times.
- Transmit descriptors are used in order 0, 1, 2, 3 after reset; TSAD
  (0x60) shows TOKn set per completed descriptor.
- Whether C mode pads short frames was not tested; the driver padded to
  60 bytes itself.

The bulk step was checked with a mutant of that driver whose ring wrap
was 8192 + 16: it passed ARP and the three small pings and failed `bulk`
at ping 8, where the ring first wraps, with its own log naming a bad
receive header at ring offset 1356.

## Robustness grind log (2026-11-14)

Regrinding against the strengthened NIC test (robustness contract,
`docs/design/robust-nic.md`). Sources used, in addition to those above:
the same RTL8139C(L)+ datasheet (newly cited: 6.5 RST resets the buffer
pointers, 6.6, 6.7 ISR bits including RDU and FOVW, 6.13 MSR reserved
bits, 6.32 TPPoll, 6.33 C+CR bit 8, 9.2 fill order); and, as trusted
project files read only, `portals/wit/portals.wit` (ethernet contract),
`docs/design/robust-nic.md`, `tests/nic/README.md`, `host/README.md`,
`host/src/faults.rs` (what the injected faults do) and `shim/README.md`
(RTL8139 deny list). No other RTL8139 driver, no QEMU source, no web.

The worktree's path is so long that the QMP socket path
(`<root>/target/nic-test/qmp.sock`) exceeds the 108-byte Unix socket
limit, so QEMU refuses to start (run 1). Runs 2-4 were made through a
short symlink to the same worktree (`/tmp/rg8139`); no file in the
repository was changed for this.

### Run 1: `harness/runs/rtl8139/20261114T183954Z`: fail (environment)

- Failing step: `arp`, "the VM powered off; no ARP reply after 1
  requests". `output.log`: QEMU `-qmp unix:...qmp.sock ... is too long.
  Path must be less than 108 bytes`.
- Diagnosis: not the driver; the path length (above).
- Change: none; ran through a short symlink. Local builds before: 0.

### Run 2: `harness/runs/rtl8139/20261114T184434Z`: fail

- Unchanged driver. Every phase passed up to `faults-device-data`; then
  `recover-after-faults-device-data` failed: "first reply after 0 ms,
  then ping 13 (seq 3413) failed: no reply" (host:
  `peer-ended-early`). In this run the register window passed (1603 of
  1603 pings answered), and so the README's "ISR read 0xffff" was a
  symptom, not the cause: the driver logged ISR = 0xffff as every error
  bit set and wrote those bits back, which did no harm in QEMU.
- Diagnosis, from the driver's own log in the device-data window:
  "dropped received frame, descriptor 25, status 0x6c000800, length
  2048". 0x6c000800 is the armed command 0x80000800 (OWN, buffer size
  2048) with its top byte changed by an injected fault: the device had
  not filled descriptor 25, but one corrupted read made it look done.
  The driver re-armed it and moved to 26. The device then filled 25
  while the driver waited on 26: the two were permanently one slot out
  of step, and each lap of the 32-slot ring the frame in the skipped
  slot was lost (or handed up late). With one ping at a time, a lost
  request is a failed recovery ping, here the 13th. Other single reads
  were also trusted: corrupted RX buffers were handed up (the
  host counted 40 `frame-malformed`, against 17 from the hostile phase
  alone in the passing runs; those it logged in the window are ICMP
  checksum failures), and TX status
  reads showed impossible error bits (`0x70e20062`).
- Change (driver redesign; see SPEC.md "Robustness contract"):
  descriptors and frame bytes are used only when two reads agree;
  registers are read up to 4 times before all-ones is believed; the RX
  ring is scanned when idle and the index resynchronised to the oldest
  done descriptor if the driver and device are out of step; a 200 ms
  health check of CR, C+CR and RCR, with a rebuild (software reset and
  reprogram, same buffers) if the configuration is lost, the device
  comes back after reading all-ones, or TX makes no progress for 1.5 s;
  RX descriptors re-armed whole and their buffer address checked;
  repeated warnings rate limited. Local builds before run 3: 1 (clean).

### Run 3: `harness/runs/rtl8139/20261114T184846Z`: pass

- Every step passed, including all three fault windows (every ping
  during the device-data and combined windows answered: 1168 of 1168,
  452 of 452) and `final-bulk`. Host: 0 violations, 0 driver errors,
  1480 device-data and 13 register faults injected, DMA 143360 bytes
  from `start` to the end.
- `frame-malformed`: 17, all before the first fault window (the
  hostile frames); none from corrupted reads.
- Driver log: 10 "RX descriptor reads disagree" (the agreed read caught
  the corruption and retried), 8 "dropped received frame" (the
  oversized hostile frames, 1519 and 1604 bytes with FCS), 2 ISR
  warnings. No resync and no rebuild was needed: the agreed reads kept
  the driver in step. Only 13 register faults hit, because registers
  are now read only every 200 ms (and in `link-up`); so the all-ones and
  rebuild paths were not exercised by this test run.
- Local builds before: 0.

### Run 4: `harness/runs/rtl8139/20261114T184945Z`: pass

- Confirmation, same driver: same result (1484 device-data and 13
  register faults, 0 driver errors, same driver warnings).

### Observations of device behaviour (robustness runs)

- During the bulk and flood phases ISR reads `0x0015` (ROK, RDU, TOK)
  repeatedly even while descriptors are being handed back: RDU alone
  is not a sign of a stall, so the driver only logs it.
- During the three link flaps no rebuild was triggered: CR kept RE and
  TE, C+CR and RCR kept their values, and TX descriptors kept
  completing (no 1.5 s stall) with the link down.
