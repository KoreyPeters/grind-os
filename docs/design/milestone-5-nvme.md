<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Milestone 5 design: an NVMe driver in the QEMU VM

Status: **approved by the human on 2026-11-14.** Facts
about NVMe come from FreeBSD's `nvme(4)` driver and SPDK's `nvme_spec.h`,
both BSD-licensed, at the commits pinned under "Sources", and are cited as
`file:line`. The NVMe specification is not used as a source (see "Sources").

## Goal

From CLAUDE.md: a Wasm NVMe driver for QEMU's emulated NVMe controller,
backed by a scratch disk image, passes a block conformance test. It reports
block size and capacity, writes patterns and reads them back exactly (first
block, last block and multi-block transfers), and flushes. Commands go
through the shim (rule 10), and a refused command fails the run.

## What the VM shows (probed with QEMU 6.2.0)

| Fact | Value |
| --- | --- |
| PCI ID, class | `1b36:0010`, `0x010802` (NVMe); subsystem `1af4:1100` |
| BAR0 | 16 KiB, 64-bit memory |
| Linux driver in the guest | none bound (our initramfs has no `nvme` module) |
| On the root bus (`00:02.0`) | **no reset method**: the shim's reset-at-open (#2) would refuse it |
| Behind a `pcie-root-port` (`01:00.0`) | `reset_method: bus`, alone in its IOMMU group |
| QEMU defaults | `max_ioqpairs` 64, `mdts` 7, no CMB (`cmb_size_mb` 0), no PMR |

So the VM puts the controller behind its own PCIe root port, and the shim's
reset policy stays as it is.

## Safety model (rule 10)

The 82574L could only be damaged through register writes. An NVMe
controller can also be damaged through **commands** it fetches from memory.
The admin opcodes include Firmware Image Download (11h), Firmware Commit
(10h, FreeBSD's `NVME_OPC_FIRMWARE_ACTIVATE`), Namespace Management (0Dh),
Format NVM (80h), Security Send (81h) and Sanitize (84h)
(`nvme.h:1080-1124`), and admin opcodes from C0h up are vendor-specific
(SPDK `nvme_spec.h:1803`). Set Features has a Save bit (CDW10 bit 31,
`nvme.h:782`) and a "feature not saveable" status (`nvme.h:1025`), so a
setting can be made to outlast the command. The IOMMU stops none of this.
Rule 10 gives three defences.

### 1. Who owns what

| Thing | Owner | Why |
| --- | --- | --- |
| Controller enable (CC), admin queue registers (AQA, ASQ, ACQ) | shim | whoever owns the admin queue can issue any admin command |
| Admin submission and completion queues | shim | admin commands are the dangerous ones |
| **All** I/O submission queues | shim | the device fetches I/O commands from them |
| **All** I/O completion queues | shim | the shim must learn from the device, not the driver, how far each submission queue has been fetched (see "Why the shim owns the completion queues") |
| Data buffers | driver | the device reads and writes data there |
| Every doorbell | shim | ringing them submits commands and returns completion entries |
| INTMS, INTMC | shim (unused) | interrupt masks; drivers poll, so the human left them off the list |

The shim brings the controller up after its VFIO reset, in FreeBSD's order
(`nvme_ctrlr.c:289-424`): clear CC.EN and wait for CSTS.RDY to clear;
write ASQ, ACQ, then AQA; then write CC (NVM command set, 4 KiB pages,
64-byte SQ entries, 16-byte CQ entries) with EN set, and wait for
CSTS.RDY. The wait is CAP.TO × 500 ms (`nvme_ctrlr.c:1896-1899`). The
driver never resets or reconfigures the controller. A wedged controller is
recovered by the next run's reset at open.

### 2. Register write list

An **allow list from a reference driver** (rule 10's third kind), taken
from FreeBSD's source. In normal operation FreeBSD writes only these
registers:

| Register | FreeBSD | Here |
| --- | --- | --- |
| CC | `nvme_ctrlr.c:346, 424, 2089` | shim only |
| ASQ, ACQ, AQA | `nvme_ctrlr.c:387, 388, 396` | shim only |
| INTMS, INTMC (0Ch, 10h) | `nvme_ctrlr.c:1537, 1540` | nobody: drivers poll |
| SQ tail doorbells | `nvme_qpair.c:1085` | shim only |
| CQ head doorbells | `nvme_qpair.c:480` | shim only |

FreeBSD never writes NSSR, the CMB, PMR (persistent memory) or boot
partition registers (`nvme.h:817-853`), so the list leaves them out, and
every other write is refused.

So the list is empty: drivers may write no register at all. (FreeBSD's
INTMS and INTMC writes were candidates; the human left them out on
2026-11-14 because drivers poll.)

Doorbell offsets, for the shim: FreeBSD stores `dstrd = CAP.DSTRD + 2`
(`nvme_ctrlr.c:1888`) and puts queue *y*'s SQ tail doorbell at
`1000h + (y << (dstrd + 1))` and its CQ head doorbell `1 << dstrd` after
it (`nvme_qpair.c:622-625`). The shim requires CAP.DSTRD = 0 and refuses
the controller otherwise; QEMU 6.2.0 reports 0.

### 3. Command portal

Drivers never write commands where the device fetches them. They hand
64-byte commands to the shim, which checks each one, **copies** it into its
own queue (so the driver can't change it after the check) and rings the
doorbell. The command layout is `struct nvme_command` (`nvme.h:890-921`).

Checks on every command:

- FUSE = 0, normal (`nvme.h:240-246`); PSDT = 0, PRPs and not SGLs
  (`nvme.h:248-253`).
- Metadata Pointer = 0 (no separate metadata).
- The opcode is on the allow list for that queue type (below), with its
  conditions.
- **Device addresses** (rule 10): PRP1 and PRP2 must lie inside one of the
  driver's DMA buffers, for the full length of the transfer. **PRP lists are
  refused.** Above two PRP entries, PRP2 points to a list of further
  addresses (`nvme_qpair.c:573-575, 1109-1117`). The list lives in driver
  memory, so the driver could change it after the check. Without lists, a
  transfer is at most two 4 KiB pages: 8 KiB, or 16 blocks of 512 bytes.
  That is enough for multi-block tests in Phase 0. Rule 10 allows lists
  later if the shim checks them and copies them into its own memory.

Admin allow list (opcodes from `nvme.h:1080-1124`). Every other opcode is
refused:

| Opcode | Command | Condition |
| --- | --- | --- |
| 06h | Identify | CNS 1 (controller) or 0 (namespace) only, the two FreeBSD uses (`nvme_ctrlr_cmd.c:49, 65`); data buffer checked |
| 09h | Set Features | Number of Queues only (FID 07h, `nvme.h:1158`; FreeBSD's only use at start-up, `nvme_ctrlr_cmd.c:203-211`); Save (bit 31) clear |
| 0Ah | Get Features | Number of Queues only, no data buffer (the data lengths of other features are not in the BSD sources) |
| 05h | Create I/O Completion Queue | PRP1 must be 0: **the shim allocates the queue**; physically contiguous (PC, `nvme.h:262`); interrupts disabled (IEN clear, `nvme.h:263`) |
| 01h | Create I/O Submission Queue | PRP1 must be 0: **the shim allocates the queue** and fills it in |
| 04h, 00h | Delete I/O CQ / SQ | the shim frees a queue's memory after the delete completes |

Every field a rule does not name must be zero. For Read and Write that
means no options: CDW12 bits 31:16 and CDW13–15 are zero, so the shim
relies only on the NLB field (CDW12 bits 15:0, SPDK `nvme_spec.h:1391`).

I/O allow list (`nvme.h:1128-1131`): Flush (00h), Write (01h), Read (02h).
Every other opcode is refused, including Write Zeroes, Dataset Management,
Write Uncorrectable and any vendor-specific opcode.

Admin commands are synchronous: the shim polls its admin CQ and returns
the 16-byte completion to the driver. I/O commands are asynchronous: the
driver submits them, then collects completions with `complete(cqid)`,
which returns copies of the new entries from the shim's completion queue.

### Why the shim owns the completion queues

Found in review before the first commit; approved by the human on
2026-11-14. The first version of this design let the driver own its I/O
completion queues and report, with each submit, the latest SQ head it had
seen. A lying driver could then report a head the device had not reached,
and the shim would overwrite a submission-queue entry the device might be
fetching at that moment. The device could read part of the old command
and part of the new one. Both had passed the checks, but the mix had not:
bytes of two valid PRP addresses can combine into the address of the
shim's admin queue, and the length of one command with the PRP1 offset of
the other can make the device treat PRP2 as a PRP list in driver memory.
Either lets the device write driver-chosen bytes into the shim's admin
queue.

So the shim owns every completion queue. It reads each completion itself,
and the SQHD field (`struct nvme_completion`, `nvme.h:926-940`) tells it
how far the device has fetched that submission queue. It never writes an
entry between that head and its tail, and `submit` returns `queue-full`
until the driver collects completions. The driver cannot forge a
completion, because it cannot write the shim's memory. The admin queue
has no such risk: the shim waits for each admin command to complete
before reusing its slot.

A refused command logs `command-refused` (opcode, which check failed,
expected, got), counts as a violation, and fails the run, just like a
denied register write.

## Interfaces (WIT sketch)

```wit
/// Portal: submit NVMe commands through the shim (rule 10).
interface nvme {
    enum error {
        refused,        // logged; the run fails
        unknown-queue,
        queue-full,
        timeout,        // admin command did not complete in time
    }
    /// Run one admin command and return its 16-byte completion entry.
    admin: func(command: list<u8>) -> result<list<u8>, error>;
    /// Queue one I/O command on submission queue `sqid`.
    submit: func(sqid: u16, command: list<u8>) -> result<_, error>;
    /// Take the new completions from completion queue `cqid`.
    complete: func(cqid: u16) -> result<list<list<u8>>, error>;
}

/// Exported by block drivers (world `block`).
interface block {
    block-size: func() -> u32;
    capacity: func() -> u64;   // in blocks
    read: func(lba: u64, count: u32) -> result<list<u8>, string>;
    write: func(lba: u64, data: list<u8>) -> result<_, string>;
    flush: func() -> result<_, string>;
}

world nvme-driver {
    include driver;   // mmio (BAR0, write-listed), dma, clock, log
    import nvme;
}

world block-driver {
    include nvme-driver;
    export block;
}
```

As built, the `nvme` error enum also has `failed`: the controller or the
admin queue failed earlier in the run (already logged).

The `nvme` portal is the first device-class portal. The host links it only
for NVMe devices and rejects it everywhere else.

## Conformance test (`tests/block`)

An independent oracle, like `nic-peer`: the **disk image itself**.

1. `check.sh` creates a scratch image (16 MiB) filled with a deterministic
   seed pattern (each block's bytes derived from its LBA).
2. In the VM, `grind-host --exercise-block` calls the driver's exports:
   - block size and capacity match the image;
   - reads of the first block, the last block and a 16-block run return the
     seed exactly;
   - writes of a new pattern to the first block, the last block and a
     16-block run, then flush, then read back exactly.
3. After the VM powers off, `check.sh` reads the image file: the written
   ranges hold the new pattern, and **every other block still holds the
   seed**. This catches a driver that fakes reads from memory, misses the
   disk, or writes to the wrong place.

Negative controls (each must fail, with a readable reason):

| Module | Expected failure |
| --- | --- |
| in-memory fake (never touches the device) | seed reads wrong; image unchanged |
| submits Format NVM (80h) | `command-refused`, run fails |
| Set Features with Save set | `command-refused` |
| vendor-specific admin opcode (C0h) | `command-refused` |
| unlisted I/O opcode (Write Zeroes, 08h) | `command-refused` |
| PRP outside its buffers | `command-refused` (address) |
| PRP list | `command-refused` |
| completion queue in its own buffer | `command-refused` |
| writes an SQ tail or CQ head doorbell directly | `mmio-denied` |
| writes CC | `mmio-denied` |

The portal-level controls are built in `tests/portals` (see its README);
the block test adds the in-memory fake.

## VM changes

- `vm/run.sh --nvme IMAGE`: `-device pcie-root-port,id=rp1,chassis=1`,
  `-drive file=IMAGE,if=none,id=nvm,format=raw`,
  `-device nvme,serial=grind0001,drive=nvm,bus=rp1`.
- `vm/init` takes a device selector, `grind.device=1b36:0010` on the
  kernel command line (default the e1000e), and handles a device with no
  Linux driver bound. Done in commit `552854f`.

## The cost

The shim gains NVMe-specific code (controller bring-up, queue ownership,
completion handling, command checks): about the size of the existing shim
again, all trusted. That is what rule 10 asks for once a device takes
commands from memory. The driver still does the device work: reading CAP
and Identify data, sizing and creating queues, building commands and PRPs,
matching completions to commands, decoding status, and splitting
transfers.

## Sources

| Source | Version | License | Used for |
| --- | --- | --- | --- |
| FreeBSD `sys/dev/nvme/nvme.h` | `freebsd/freebsd-src` commit `74d72b3491e810068313e49629c3b8fe46f29318`, sha256 `e9ee9a96fae349db7b78b8b91455b94c538f2f1080554fab6711269b86e6d036` | BSD-2-Clause | facts: register layout, command layout, opcodes, feature IDs, field bits |
| FreeBSD `sys/dev/nvme/nvme_ctrlr.c` | same commit, sha256 `d54ff501a0a5f8993ed518f7d9625a149ffdecf1b71cc73e0342e3d6ab7dbd40` | BSD-2-Clause | facts: bring-up order, timeout, doorbell stride, which registers it writes |
| FreeBSD `sys/dev/nvme/nvme_qpair.c` | same commit, sha256 `44990e079e6fe234c068c2f1799fbbdc5406f591bb055a08a976440cd2413491` | BSD-2-Clause | facts: doorbell offsets, PRP usage |
| FreeBSD `sys/dev/nvme/nvme_ctrlr_cmd.c` | same commit, sha256 `8287a9d13f5dff894c1c4ef7ae5915d11c7d3901b6fbb650d485d11627467c43` | BSD-2-Clause | facts: queue-creation and Number of Queues commands |
| SPDK `include/spdk/nvme_spec.h` | `spdk/spdk` commit `91404c6437e45395a943bfeaf48f58ea1d6aff74`, sha256 `244249b1d9b560484851b804de8155f371de4e5d80e5f9719b2db5006a3ef01c` | BSD-3-Clause | facts: admin vendor-specific opcodes start at C0h |
| QEMU 6.2.0, observed in the test VM | — | — | the table at the top |

Only facts are taken; no code is copied. The files are kept in the
gitignored `.cache/sources/`. FreeBSD's `nvme_ns_cmd.c` and
`nvme_private.h` were also fetched and only read.

**Not used as a source:** the NVM Express Base Specification, revision
1.4c (sha256 `ff92005660e8b58e248b2ff1416acc8c8c1aba1b3b7c14f6de9b80f2181649bc`).
It was read while drafting an earlier version of this design. Its notice
gives non-members only the right to review and cite it, so the human
decided not to rely on it. Every fact above was re-checked against the
BSD sources, and the claims found only in the specification were dropped.
Two of those claims were the vendor-specific register range and the
vendor-specific I/O opcode range. The design doesn't need either, because
both lists refuse everything they don't name.

## Decisions (2026-11-14)

1. The specification's licence notice: facts come from FreeBSD and SPDK,
   not the specification.
2. Rule 10 now allows command allow lists from a reference driver's
   source where rule 6 permits it.
3. No PRP lists in Phase 0: 8 KiB per command.
4. Trusted code grows by an NVMe-specific shim module.
5. Order of work: the VM, the portal and shim, the host and block test,
   and the harness mapping, as separate commits, each with its negative
   controls; then `drivers/nvme` SPEC and PROVENANCE, then grinding.
