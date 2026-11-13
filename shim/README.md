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

### Intel 82574L (QEMU e1000e): PENDING

The list is **empty and unreviewed**. Its entries must come from the Intel
82574 GbE Controller Family datasheet, which could not be read yet in the
environment where this was built:

- Intel no longer serves the datasheet publicly. Its old URLs return 403 or
  404.
- A copy of Intel's PDF was obtained from RS Components
  (`https://docs.rs-online.com/96e8/0900766b81384733.pdf`, sha256
  `bca28652978d7024830c5a6dfad9105f118d5b9a5b540b1ffa994bfc7178bea2`,
  revision not yet confirmed). The Intel Specification Update 320709
  rev 4.1 was obtained from
  `https://cdrdv2-public.intel.com/320709/82574-specupdate-rev4-1-external-320709.pdf`
  (sha256 `8091835a9701b23d3b5ef0a53149c1cf6e97df715945f0a15bc72a0bd99d627d`).
  Both are kept in the gitignored `.cache/datasheets/` and are not
  redistributed.
- No PDF text extraction tool is available yet. See the milestone 3 notes.

Besides the BAR0 entries, the shim already keeps the 82574's indirect paths
to NVM and flash away from drivers. The flash BAR (BAR1) and the I/O-port
BAR are never exposed, because the I/O BAR's `IOADDR`/`IODATA` pair reaches
every register. Config space is never exposed either.

## Sources

- VFIO and PCI constants: Linux userspace API headers `<linux/vfio.h>` and
  `<linux/pci_regs.h>` (GPL-2.0 WITH Linux-syscall-note). Values were checked
  by compiling a C program against the host's headers and printing them.
- No Linux driver code was consulted.
