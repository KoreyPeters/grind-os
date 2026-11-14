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
