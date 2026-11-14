<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# grind-shim

The only code that touches a device on a driver's behalf. Trusted.

- `Device::open` opens a PCI device through VFIO (Type1 v2 IOMMU), confirms
  its identity through config space, enables memory decode and bus
  mastering, and finds BAR0. Each failure says which step failed, what was
  expected and what happened.
- `Window` gives checked 32-bit access to BAR0. Writes that touch a
  deny-list entry are refused, logged (`mmio-denied`) and counted. Any count
  above zero fails the run.
- `DmaAllocator` and `DmaBuffer` handle DMA memory. Buffers are page-aligned,
  zeroed, pinned and mapped with `VFIO_IOMMU_MAP_DMA` at IOVAs from 4 GiB up,
  with an unmapped guard page after each one. Dropping a buffer unmaps it
  before freeing it. If the unmap fails, the memory is leaked and logged
  rather than freed while the device might still reach it.
- `log` writes JSON lines, one per event.

No crate dependencies. `unsafe` is limited to the ioctl wrappers in
`sys.rs`, adopting the device fd, and DMA buffer memory in `dma.rs`.

## Deny lists (rule 10)

`src/deny.rs` holds one list per supported device model. A device with no
list cannot be opened. A list that is not yet human-reviewed is refused
unless the caller passes `allow_unreviewed`, which is meant for emulated
devices only (`grind-host --allow-unreviewed-deny-list`).

Writes are the only thing filtered: reads are allowed everywhere in BAR0.
Every entry must cite the datasheet section it comes from.

### Intel 82574L (QEMU e1000e): written, awaiting human review

13 BAR0 registers are denied. Each entry in `src/deny.rs` cites its
datasheet section:

| Offset | Register | Section | Why |
| --- | --- | --- | --- |
| 0x00010 | EEC | 10.2.2.3 | bit-bangs the NVM; `FWE` enables Flash write/erase; `AUPDEN` enables autonomous Flash update |
| 0x0001C | FLA | 10.2.2.6 | bit-bangs the Flash; `FL_ER` sends an erase command |
| 0x01010–0x01020 | EEMNGCTL, EEMNGDATA, FLMNGCTL, FLMNGDATA, FLMNGCNT | 10.2.2.18–22 | firmware's NVM/Flash path; read-only to software per the datasheet, denied as defence in depth |
| 0x01028 | FLASHT | 10.2.2.23 | timer for autonomous Flash updates |
| 0x0102C | EEWR | 10.2.2.24 | writes a word to the NVM, even without a valid signature |
| 0x01030–0x01038 | FLSWCTL, FLSWDATA, FLSWCNT | 10.2.2.25–27 | software Flash burst writes |
| 0x0103C | FLOP | 10.2.2.28 | opcodes the device uses to drive the NVM |

The following were checked and deliberately allowed, because a driver needs
them and they only read NVM or are volatile:

- EERD (0x14): a write only starts a word read.
- CTRL_EXT (0x18): `EE_RST` only re-reads the NVM.
- MDIC (0x20).
- EXTCNF_CTRL (0xF00): MDIO ownership.
- FLOL (0x1050): RAM power gating; its other bits are loaded *from* NVM.
- SWSM (0x5B50): a semaphore.

The list stays `reviewed: false` until the human reviews it against the
datasheet (rule 10).

**Sources.** Intel no longer serves the datasheet publicly; its URLs return
403 or 404.

- Intel 82574 GbE Controller Family Datasheet, **revision 2.5 (April
  2009)**. Intel's PDF, obtained from RS Components
  (`https://docs.rs-online.com/96e8/0900766b81384733.pdf`, sha256
  `bca28652978d7024830c5a6dfad9105f118d5b9a5b540b1ffa994bfc7178bea2`).
  Used: Table 77 (register summary), sections 10.1.1 (address decoding) and
  10.2.2–10.2.3 (register descriptions). A later revision 3.4 (2014) exists
  but was not obtained.
- Intel Specification Update 320709 rev 4.1 (February 2020), from
  `https://cdrdv2-public.intel.com/320709/82574-specupdate-rev4-1-external-320709.pdf`
  (sha256 `8091835a9701b23d3b5ef0a53149c1cf6e97df715945f0a15bc72a0bd99d627d`).
  It lists no documentation changes. Its NVM items concern NVM *contents*,
  not register behaviour.

Both are kept in the gitignored `.cache/datasheets/` and are not
redistributed.

Besides the BAR0 entries, the shim keeps the 82574's indirect paths
to NVM and flash away from drivers. The flash BAR (BAR1) and the I/O-port
BAR are never exposed, because the I/O BAR's `IOADDR`/`IODATA` pair reaches
every register. Config space is never exposed either.

## Sources

- VFIO and PCI constants: Linux userspace API headers `<linux/vfio.h>` and
  `<linux/pci_regs.h>` (GPL-2.0 WITH Linux-syscall-note). Values were checked
  by compiling a C program against the host's headers and printing them.
- No Linux driver code was consulted.
