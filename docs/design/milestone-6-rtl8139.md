<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Next milestone design: an RTL8139 network driver in the QEMU VM

Status: **approved by the human on 2026-11-14** (all five recommendations).
Section numbers refer to the RTL8139C(L)+ datasheet (see "Sources").

## Goal

A second network driver, for a different chip, ground mostly from its
datasheet alone. Proposed milestone text for CLAUDE.md (your edit):

> **Third device, from a datasheet.** A Wasm driver for QEMU's emulated
> Realtek RTL8139 passes the NIC conformance test (link up, answer a
> ping). The driver and its SPEC.md are written by a fresh session from
> the datasheet alone, grinding against the harness; the session records
> each failing run and what it changed.

Why this device: the NVMe driver passed on its first run twice, because
the shim and a detailed BSD-derived spec left little to get wrong. A
datasheet-only driver for an older, quirkier chip is a fairer test of the
grind loop, and it reuses `tests/nic` and the deny-list model as they
are, so the trusted work is small.

## What the VM shows (probed with QEMU 6.2.0)

| Fact | Value |
| --- | --- |
| PCI ID, class | `10ec:8139` rev 0x20, `0x020000` (Ethernet); subsystem `1af4:1100` |
| BAR0 | **I/O ports**, 256 bytes |
| BAR1 | memory, 256 bytes (the same registers) |
| Expansion ROM BAR | 512 KiB (never exposed) |
| On the root bus | **no reset method**: the shim's reset-at-open would refuse it |
| Behind a `pcie-root-port` | `reset_method: bus`, alone in its IOMMU group |
| Linux driver in the guest | none bound (the initramfs has no `8139too`) |

As with NVMe, the VM puts the device behind its own PCIe root port.

## Sources

- **RTL8139C(L)+ datasheet, Rev. 1.6, 03 September 2004, Track ID
  JATR-1076-21**, Realtek's PDF from
  `https://people.freebsd.org/~wpaul/RealTek/spec-8139cp(160).pdf`
  (sha256 `f3973ef7d9f6cc9cf0bc372c19323a383d38c6f1f4465063d1be885fc83a38cc`).
  QEMU's `rtl8139` models the C+ (revision 0x20).
- **RTL8139C(L) datasheet, Rev. 1.6, 29 December 2005**, same page
  (sha256 `9c0a6bd3ae35551f9bd024443776b5b5a7797527c163e6940389821dff47e93b`),
  for cross-checking.

Both are kept in the gitignored `.cache/datasheets/rtl8139/`. (A first
download was truncated; the files above match the server's
Content-Length and the PDF's own linearization length.)

## Safety model (rule 10)

No command queues: the device fetches only packet buffers and, in C+
mode, descriptors from driver memory, both inside the IOMMU. So this is
register writes only, as for the 82574L: **a deny list from the
datasheet**. The register map (section 5) shows two paths to non-volatile
memory:

| Offset | Register | Section | Why |
| --- | --- | --- | --- |
| 0x50 | 9346CR | 6.10 | EEM = 10b is "93C46 Programming": bits 3-0 then bit-bang the EEPROM. EEM = 11b unlocks writes to CONFIG0/1/3/4 and BMCR bits 13, 12, 8 |
| 0xD4-0xD7 | FLASH | 6.30 | "Flash memory read/write register": drives the boot ROM flash's data and control pins |

Proposed list: deny 9346CR and FLASH outright, plus CONFIG0, CONFIG1,
CONFIG3, CONFIG4 and Config5 as defence in depth (their values are
loaded from the EEPROM; with 9346CR denied they cannot be unlocked
anyway). The I/O BAR and the expansion ROM BAR are never exposed. I will
read every row of the register map (section 5) and every register
description before writing the list, cite each entry's section, and
record what was checked and allowed, as for the 82574L. You review it
before any driver runs without `--allow-unreviewed-deny-list`.

## Trusted changes

1. **The register window is BAR1 for this device.** The shim maps BAR0
   today; here BAR0 is the I/O-port window, which the portals never
   expose. Proposal: each write list names its register BAR (`bar: 0`
   for the e1000e and NVMe, `bar: 1` here), and the shim checks in
   config space that it is a memory BAR before mapping it. `mmio`'s
   docs change from "BAR0" to "the device's memory register BAR".
2. **VM:** `vm/run.sh --nic rtl8139` swaps the e1000e for an RTL8139 behind
   its own root port, same MAC and network backend (so `--net-udp` and
   `nic-peer` work unchanged), and passes `grind.device=10ec:8139`.
   `vm/init` learns `10ec:8139` (no Linux driver).
3. **Harness:** `rtl8139 → tests/nic`, with `--nic rtl8139` passed to the
   VM. `tests/nic` itself does not change.
4. **Portal cases:** a third boot in `tests/portals` with the RTL8139:
   reset at open, quiesce, BAR1 mapped, and denied writes to 9346CR and
   FLASH (`mmio-denied`, the run fails). The existing `silent` module is
   the NIC test's negative control on this chip too.

## Decisions (2026-11-14)

1. The datasheet's notice: accepted; facts only.
2. Datasheet only; FreeBSD's `rl(4)` is the fallback if the grind stalls,
   recorded in PROVENANCE if used.
3. `mmio` gains `read8`, `read16`, `write8` and `write16`.
4. A fresh, isolated session writes SPEC.md, PROVENANCE.md and the driver
   from the datasheet and grinds, recording every failing run. Claude
   builds only the trusted pieces.
5. Order of work, as separate commits with negative controls: VM; shim
   BAR selection and the deny list (human review); `mmio` widths and
   portal cases; harness mapping; then the fresh-session grind.
