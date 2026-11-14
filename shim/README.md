<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# grind-shim

The only code that touches a device on a driver's behalf. Trusted.

- `Device::open` opens a PCI device through VFIO (Type1 v2 IOMMU), resets
  it (`VFIO_DEVICE_RESET`, so nothing carries over from a previous user; a
  device VFIO cannot reset is refused), confirms its identity through config
  space, enables memory decode and bus mastering, and finds the register
  BAR the device's write list names (`bar`: BAR0 for the e1000e and NVMe,
  BAR1 for the RTL8139). It refuses a BAR that config space marks as I/O
  ports. Each failure says which step failed, what was expected and what
  happened.
- `Device::quiesce` clears bus mastering and checks it reads back clear, so
  the device can no longer issue DMA. The host calls it before the
  driver's DMA buffers are unmapped; dropping a `Device` calls it as a
  backstop. Logged as `device-quiesce` (command register before and after)
  or `device-quiesce-error`.
- `Window` gives checked access to that BAR. Writes the device's write
  list refuses are not performed; they are logged (`mmio-denied`) and
  counted. Any count above zero fails the run.
- `DmaAllocator` and `DmaBuffer` handle DMA memory. Buffers are page-aligned,
  zeroed, pinned and mapped with `VFIO_IOMMU_MAP_DMA`, with an unmapped
  guard page after each one. IOVAs are never reused. At open the shim asks
  the IOMMU which IOVAs it will map (`VFIO_IOMMU_GET_INFO`, the IOVA-range
  capability, which excludes reserved regions such as the MSI window) and
  logs them as `dma-regions`. `allocate` takes IOVAs from 4 GiB up to the
  end of that usable range; `allocate32` takes them from 1 MiB up to the
  first reserved hole below 4 GiB, for devices that can only address 32
  bits (issue #13). On QEMU 6.2.0 the usable ranges are `0x0-0xfedfffff`
  and `0xfef00000-0x7fffffffff`, so the regions are `0x100000-0xfee00000`
  and `0x100000000-0x8000000000`. If the kernel reports no ranges, the
  high region is assumed as before (up to 2^39) and `allocate32` fails
  with `out-of-memory`, logged. Dropping a buffer unmaps it
  before freeing it. If the unmap fails, the memory is leaked and logged
  rather than freed while the device might still reach it.
- For a device that fetches commands from memory (NVMe), `Device::open`
  also brings the controller up (`nvme.rs`): it checks CAP, disables the
  controller, sets up the admin queue in shim-owned DMA memory and enables
  it, logging `nvme-ready`. `Device::nvme_admin` and `Device::nvme_submit`
  check each driver command against the device's command list and the
  driver's own DMA buffers, copy it into a shim-owned queue and ring the
  doorbell. `Device::nvme_complete` reads new entries from a shim-owned
  completion queue, learns each submission queue's head from them, and
  returns copies. A refused command is logged (`command-refused`) and counted;
  any count above zero fails the run.
- The DMA allocator remembers which live buffers are the driver's, so the
  shim can tell a driver's addresses from its own queues. IOVAs are never
  reused.
- `log` writes JSON lines, one per event.

No crate dependencies. `unsafe` is limited to the ioctl wrappers in
`sys.rs`, adopting the device fd, and DMA buffer memory in `dma.rs`.

## Write lists (rule 10)

`src/deny.rs` holds one write list per supported device model. Each list
has a kind (CLAUDE.md rule 10), logged in `device-open` as `list_kind`:

| Kind | Built from | A write passes when | Each entry cites |
| --- | --- | --- | --- |
| `deny` | the device's datasheet | it touches no entry | a datasheet section |
| `allow` | a reference driver's source (if rule 6 permits it) or traces of it | it lies entirely inside one entry | a file at an exact commit (`... at commit <hash>`), or a trace (`trace <file>`) |

A device that takes commands also has a command list (below). It counts as
reviewed only when both lists are.

Entries are naturally aligned registers of 1, 2 or 4 bytes, or whole
dword ranges. A write is checked against every byte it touches, so a
32-bit write that covers a denied byte register is denied.

A refused write logs `mmio-denied` with the entry it hit (deny list) or
`register: unlisted` (allow list), plus `list_kind`. An allow list with no
entries refuses every write. Unit tests cover both kinds, including writes
that straddle an entry's edge; `every_list_is_well_formed` checks that
every shipped list is aligned, sorted, non-overlapping, has reasons, and
cites sources of the right form for its kind.

A device with no list cannot be opened. A list that is not yet human-reviewed is refused
unless the caller passes `allow_unreviewed`
(`grind-host --allow-unreviewed-deny-list`) *and* the machine is an
emulator: `/sys/class/dmi/id/sys_vendor` must read exactly `QEMU`. On real
hardware the flag is refused, and the refusal names the vendor it found.
`device-open` logs the value as `dmi_sys_vendor`.

DMI describes the machine, not the device: a real device passed through
to a QEMU guest would pass that check. So the device must also be one
QEMU emulates (issue #12): its vendor and device IDs and its subsystem
IDs (`/sys/bus/pci/devices/<bdf>/subsystem_vendor`, `subsystem_device`)
must exactly match an entry of `EMULATED_DEVICES` in `src/deny.rs`:

| Device | IDs | Subsystem |
| --- | --- | --- |
| QEMU e1000e (82574L) | 8086:10d3 | 8086:0000 |
| QEMU rtl8139 | 10ec:8139 | 1af4:1100 |
| QEMU NVMe | 1b36:0010 | 1af4:1100 |

1af4:1100 is the subsystem QEMU gives devices that set none of their own
(Red Hat's vendor ID, QEMU's subsystem ID); real hardware does not carry
it. A real card carries its maker's or its OEM's subsystem (an OptiPlex
NIC, Dell's 1028), so it is refused, and the refusal names the IDs it
found. Unreadable subsystem IDs are refused too. `device-open` logs them
as `subsystem`; in the test VM they read as in the table.

Limit: QEMU can be told to give a passed-through device any subsystem IDs
(vfio-pci's `x-pci-sub-vendor-id`, `x-pci-sub-device-id`). That defeats
the check on purpose; it guards against mistakes, not against someone
set on running an unreviewed list on real hardware. A reviewed list is
still required before passing real hardware through.

Writes are the only thing filtered: reads are allowed everywhere in BAR0.

### QEMU NVMe controller: register allow list

`QEMU_NVME` in `src/deny.rs` is an allow list from a reference driver's
source (rule 10's third kind): FreeBSD's nvme(4). In normal operation
FreeBSD writes only CC, ASQ, ACQ, AQA, INTMS, INTMC, the submission-queue
tail doorbells and the completion-queue head doorbells
(`nvme_ctrlr.c:346, 387, 388, 396, 424, 1537, 1540, 2089`;
`nvme_qpair.c:480, 1085`). The shim owns CC, AQA, ASQ, ACQ and every
queue with its doorbells. That leaves INTMS and INTMC
(`nvme_ctrlr.c:1537, 1540`), the interrupt masks; drivers poll and never
need them, so the human chose to leave them out too. **The list is empty:
every driver register write is refused**, including every doorbell and
NSSR, CMB, PMR and boot-partition registers, which FreeBSD never writes. The shim's own
doorbell offsets assume CAP.DSTRD = 0 (`nvme_qpair.c:622-625`,
`nvme_ctrlr.c:1888`); it refuses the controller at open (`nvme-cap`) if
it reads anything else. QEMU 6.2.0 reports 0.

The shim owns the completion queues, not just the submission queues, so
that it learns how far the device has fetched each submission queue from
the device (each completion's SQHD) rather than from the driver. It never
overwrites an entry the device may still be reading; a torn read of two
checked commands could otherwise reach the shim's own memory. See
`docs/design/milestone-5-nvme.md`, "Why the shim owns the completion
queues".

### NVMe command list

`NVME_COMMANDS` in `src/nvme.rs` is attached to the QEMU NVMe write list.
Every other command is refused, including Firmware Image Download,
Firmware Commit, Namespace Management, Format NVM, Security Send, Sanitize
and every vendor-specific admin opcode (C0h and up). Every field a rule
does not name must be zero; FUSE, PSDT (no SGLs) and the metadata pointer
must be zero for every command.

| Queue | Opcode | Command | Rule |
| --- | --- | --- | --- |
| admin | 06h | Identify | CNS 1 (controller) or 0 (namespace) only, as FreeBSD uses (`nvme_ctrlr_cmd.c:49, 65`); a 4096-byte data buffer inside the driver's buffers |
| admin | 09h | Set Features | Number of Queues (FID 07h) only; Save clear; no data |
| admin | 0Ah | Get Features | Number of Queues only; no data |
| admin | 05h | Create I/O Completion Queue | QID 1–64, 2–256 entries; contiguous; interrupts off; PRP1 = 0, the shim allocates the queue |
| admin | 01h | Create I/O Submission Queue | QID 1–64, 2–64 entries, an existing CQ; PRP1 = 0, the shim allocates the queue |
| admin | 00h, 04h | Delete I/O SQ / CQ | an existing queue; a CQ only once its SQs are gone |
| I/O | 00h | Flush | no data, no options |
| I/O | 01h, 02h | Write, Read | no options (CDW12 bits 31:16 and CDW13–15 zero); (NLB + 1) blocks inside the driver's buffers through at most two PRP entries |

Data addresses: PRP1 covers the transfer up to the end of its page, PRP2
(page-aligned) the rest, and both ranges must lie inside the driver's live
DMA buffers. A transfer that needs more than two entries would need a PRP
list, which lives in driver memory the driver could change after the
check, so it is refused (`prp-list`). For Read and Write the shim learns
the block size by identifying the namespace itself, into its own page; a
namespace with metadata, or a block size outside 512–4096 bytes, is refused.

The human reviewed both the register list and the command list against
their sources and set `reviewed: true` in their own commit (rule 10). Any
change to either needs a new review.

**Sources.** Facts only; no code copied. The NVM Express specification is
not a source (see `docs/design/milestone-5-nvme.md`).

- FreeBSD `sys/dev/nvme/nvme.h`, `nvme_ctrlr.c`, `nvme_ctrlr_cmd.c`,
  `nvme_qpair.c` at commit `74d72b3491e810068313e49629c3b8fe46f29318`
  (BSD-2-Clause); checksums in the design doc. `nvme_ns_cmd.c` and
  `nvme_private.h` were read only.
- SPDK `include/spdk/nvme_spec.h` at commit
  `91404c6437e45395a943bfeaf48f58ea1d6aff74` (BSD-3-Clause), sha256
  `244249b1d9b560484851b804de8155f371de4e5d80e5f9719b2db5006a3ef01c`:
  vendor-specific admin opcodes from C0h (line 1803); field positions in
  CDW10–12 for queue creation and Read/Write (lines 1203-1205, 1308-1323,
  1386-1391).

All are kept in the gitignored `.cache/sources/`.

### Realtek RTL8139 (QEMU rtl8139): deny list

`RTL8139` in `src/deny.rs`, a deny list from the datasheet. Its
registers are in **BAR1** (memory); BAR0 is the same registers as I/O
ports and is never exposed, nor is the 512 KiB expansion ROM BAR.

| Offset | Register | Section | Why |
| --- | --- | --- | --- |
| 0x50 | 9346CR | 6.10 | EEM = 10b bit-bangs the 93C46/93C56 EEPROM through bits 3-0; EEM = 01b re-runs the EEPROM auto-load; EEM = 11b unlocks CONFIG0/1/3/4 and BMCR bits 13, 12, 8 |
| 0x51 | CONFIG0 | 6.11 | boot ROM size, from the EEPROM; defence in depth |
| 0x52 | CONFIG1 | 6.12 | LED and power settings, from the EEPROM; defence in depth |
| 0x59 | CONFIG3 | 6.14 | PARM_En: whether the PHY parameters are auto-loaded from the EEPROM; defence in depth |
| 0x5A | CONFIG4 | 6.15 | AnaOff turns off the analog power; LongWF survives PCI reset; defence in depth |
| 0xD4-0xD7 | FLASH | 6.30 | drives the boot ROM flash's data and address pins; SWRWEn enables software flash access |
| 0xD8 | Config5 | 6.31 | not protected by 9346CR; bits from the EEPROM; FIFOAddrPtr (Realtek internal test use) resets only at power-on |

CONFIG0/1/3/4 are writable only with 9346CR unlocked, so denying 9346CR
already protects them; they are listed too in case that protection is
not what the datasheet says.

Checked and deliberately allowed: IDR0-5 and MAR0-7 (volatile; IDR is
loaded *from* the EEPROM), MSR (6.13), BMCR (its protected bits need
9346CR), CSCR, PHY1_PARM, TW_PARM and PHY2_PARM (software can write them
only with PARM_En = 0 and 9346CR unlocked), the wakeup-frame registers
(0x84-0xD3), TPPoll, C+CR, RDSAR, ETThR, and the CardBus and MII
registers (0xF0-0xFF).

**Reserved bytes are left writable** (0x06-0x07, 0x53, 0x5B, 0x5F,
0x76-0x77, 0x81, 0xDA-0xDF, 0xE2-0xE3, 0xED-0xEF): the datasheet
documents them as reserved, and most share a dword with registers that
must be written whole (IDR0-5 "are only permitted to write by 4-byte
access"). This is a judgement for the review.

The EEPROM can also be written through PCI Vital Product Data (8.8),
which is in config space; drivers never see config space.

The human reviewed the list against the datasheet, including leaving the
reserved bytes writable, and set `reviewed: true` in their own commit
(rule 10). Any change to the entries needs a new review.

**Sources.**

- Realtek RTL8139C(L)+ Datasheet, **Rev. 1.6 (03 September 2004), Track
  ID JATR-1076-21**, Realtek's PDF from
  `https://people.freebsd.org/~wpaul/RealTek/spec-8139cp(160).pdf`
  (sha256 `f3973ef7d9f6cc9cf0bc372c19323a383d38c6f1f4465063d1be885fc83a38cc`).
  Used: the section 6 register table, 6.10-6.15, 6.30, 6.31, 8.1.4, 8.8,
  and every mention of the EEPROM, flash or boot ROM. QEMU's `rtl8139` is
  a C+ (revision 0x20).
- Realtek RTL8139C(L) Datasheet, Rev. 1.6 (29 December 2005), same page
  (sha256 `9c0a6bd3ae35551f9bd024443776b5b5a7797527c163e6940389821dff47e93b`):
  only read, to cross-check.

Both are kept in the gitignored `.cache/datasheets/rtl8139/` and read as
text with `pdftotext -layout`.

### Intel 82574L (QEMU e1000e): written

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
- POEMB (0xF10): PHY link and pause bits. Bit 7 (`Auto_update`, 10.2.12.1)
  disables Flash updates from the shadow RAM. It cannot write the Flash
  itself: an update needs a write to the shadow RAM (3.3.6.1). That write
  goes through the EEPROM interface, where bit-banging is not supported for
  the shadow RAM and EEWR is denied.
- FLOL (0x1050): RAM power gating; its other bits are loaded *from* NVM.
- SWSM (0x5B50): a semaphore.

Section 3.3.6.1 mentions a `FLUPD` bit "in the FLA register" that 10.2.2.6
does not list (bits 29:11 are reserved). FLA is denied as a whole register,
which covers it either way. Denying whole registers rather than single bits
is deliberate for this reason.

The human reviewed the list against the datasheet and set
`reviewed: true` in their own commit (rule 10). Any change to the entries
needs a new review. No test depends on the review state: the gate is unit-tested with test-only lists, and the
`deny-list-gate` portal case accepts either state as long as the host's
behaviour matches the flag.

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

- **QEMU 6.2.0** (Debian 1:6.2+dfsg-2ubuntu6.31), observed 2026-11-14:
  QMP `query-pci` on a q35 machine with `-device e1000e`, `-device
  rtl8139` and `-device nvme`, for the subsystem IDs in
  `EMULATED_DEVICES` (issue #12). Facts only; no QEMU code was used.
  Confirmed in the test VM, where `device-open` logs the same values.

- VFIO and PCI constants (including `PCI_BASE_ADDRESS_0`, added for the
  register BAR, and `VFIO_IOMMU_GET_INFO` with its IOVA-range capability,
  added for the DMA regions): Linux userspace API headers `<linux/vfio.h>` and
  `<linux/pci_regs.h>` (GPL-2.0 WITH Linux-syscall-note). Values were checked
  by compiling a C program against the host's headers and printing them.
- No Linux driver code was consulted.
