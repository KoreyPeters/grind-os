<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# nvme driver spec (QEMU's emulated NVMe controller, `1b36:0010`)

Written from FreeBSD's nvme(4) driver (BSD-2-Clause) and SPDK's
`nvme_spec.h` (BSD-3-Clause), at the commits in `PROVENANCE.md`. Every
reference below is `file:line` in one of them; FreeBSD files are under
`sys/dev/nvme/`. The NVM Express specification was not consulted. Facts
only: no code is copied.

## Scope

Milestone 5: report block size and capacity, read and write whole blocks
exactly, flush. Namespace 1 only, one I/O queue pair, one command in
flight, polling. Out of scope: interrupts, more queues, other namespaces,
metadata, protection information, Write Zeroes, Dataset Management and
every command the shim does not allow.

## Interface

World `block-driver` (`portals/wit`): exports `start` and `block`
(`block-size`, `capacity`, `read`, `write`, `flush`); imports the portals
plus `nvme` (`admin`, `submit`, `complete`).

What the shim already does (CLAUDE.md rule 10; `shim/README.md`), so the
driver does not:

- resets the device, enables the controller (CC, admin queue) and owns
  every queue and doorbell, in its own memory;
- checks every command against its list and copies it into its queue.

Allowed commands and their rules: `shim/README.md`, "NVMe command list".
In short: every field a rule does not name is zero; at most two PRP
entries (no PRP lists); data inside the driver's DMA buffers; Create I/O
CQ/SQ with PRP1 = 0.

## Registers (read only)

The driver writes no register (the write list is empty). It reads:

| Register | Offset | Fields | Source |
| --- | --- | --- | --- |
| CAP | 0x00 (64-bit, two 32-bit reads) | MQES bits 15:0 (zero-based); TO 31:24; MPSMIN at CAP_HI 19:16 | FreeBSD `nvme.h:81-118`, register layout `nvme.h:817-853` |
| CSTS | 0x1C | RDY bit 0; CFS bit 1 | `nvme.h:177-180` |

`start` fails if CSTS.RDY is not 1 or CSTS.CFS is 1: the shim should have
left the controller ready.

## Commands

Layout: `struct nvme_command`, 64 bytes (`nvme.h:890-921`): byte 0
opcode; byte 1 flags (0); bytes 2-3 CID; 4-7 NSID; 24-31 PRP1; 32-39
PRP2; 40-63 CDW10-15. All little-endian.

| Command | Opcode | Fields | Source |
| --- | --- | --- | --- |
| Identify Controller | admin 06h | CDW10 = 1; PRP1 = 4 KiB buffer | `nvme_ctrlr_cmd.c:33-49` |
| Identify Namespace | admin 06h | CDW10 = 0; NSID = 1; PRP1 = 4 KiB buffer | `nvme_ctrlr_cmd.c:55-70` |
| Set Features, Number of Queues | admin 09h | CDW10 = 07h; CDW11 = (NCQR - 1) << 16 \| (NSQR - 1); result in completion DW0 | `nvme_ctrlr_cmd.c:203-211`; `nvme.h:1158` |
| Create I/O Completion Queue | admin 05h | CDW10 = (entries - 1) << 16 \| QID; CDW11 = 1 (PC; IEN clear); PRP1 = 0 | `nvme_ctrlr_cmd.c:85-94`; `nvme.h:262-263`; SPDK `nvme_spec.h:1203-1205, 1318-1323` |
| Create I/O Submission Queue | admin 01h | CDW10 = (entries - 1) << 16 \| QID; CDW11 = CQID << 16 \| 1 (PC); PRP1 = 0 | `nvme_ctrlr_cmd.c:109-119`; `nvme.h:257-259`; SPDK `nvme_spec.h:1308-1313` |
| Read / Write | I/O 02h / 01h | NSID; CDW10 = LBA bits 31:0; CDW11 = LBA bits 63:32; CDW12 = blocks - 1 (bits 15:0) | `nvme.h:2230-2251`; SPDK `nvme_spec.h:1386-1391` |
| Flush | I/O 00h | NSID | `nvme.h:2225` |

### Completions

`struct nvme_completion`, 16 bytes (`nvme.h:926-940`): DW0 command
specific; DW2 bits 15:0 SQHD, 31:16 SQID; DW3 bits 15:0 CID, 31:16
status. Status (`nvme.h:282-293`): P bit 0, SC bits 8:1, SCT bits 11:9,
DNR bit 15. Success is SC = 0 and SCT = 0 (`nvme.h:957, 967`).

Admin completions come back from `admin`. I/O completions come from
`complete(1)`; the shim has already consumed them from its queue, so the
driver needs no phase tracking. The driver checks every completion, admin
and I/O alike: its CID (and, for I/O, its SQID) must match the command it
sent, and its status must be success. Admin commands use CID 0; I/O
commands count up from 1.

### Identify data

- Controller (`struct nvme_controller_data`, 4096 bytes, `nvme.h:1309-1584`):
  MDTS byte 77; NN bytes 516-519. Maximum transfer = `1 << (MDTS + 12 +
  CAP.MPSMIN)` bytes when MDTS > 0, unlimited when 0 (`nvme_ctrlr.c:535-537`;
  `NVME_MPS_SHIFT` 12, `nvme.h:71`).
- Namespace (`struct nvme_namespace_data`, 4096 bytes, `nvme.h:1586-1695`):
  NSZE bytes 0-7 (blocks); NLBAF byte 25 (zero-based); FLBAS byte 26; LBAF
  array from byte 128, 4 bytes each: MS bits 15:0, LBADS bits 23:16
  (`nvme.h:712-716`). Format index = FLBAS bits 3:0, plus bits 6:5 as bits
  5:4 when NLBAF + 1 > 16 (`nvme_ns_data_format_index`, `nvme.h:2268-2279`).
  Block size = `1 << LBADS`. The driver refuses MS != 0.

## Start

1. Read CAP and CSTS; require RDY = 1, CFS = 0.
2. Allocate a 4 KiB DMA buffer. Identify Controller; record MDTS; require
   NN >= 1. The same buffer serves both Identify commands and is freed
   when `start` returns.
3. Identify Namespace 1; record NSZE and the block size; require NSZE > 0,
   MS = 0 and a block size of 512 to 4096 bytes (LBADS 9 to 12, the range
   the shim checks transfers for; anything else would only be refused
   later).
4. Set Features (Number of Queues) for one SQ and one CQ. The result
   (DW0: bits 15:0 SQs, 31:16 CQs, zero-based) always grants at least one
   of each; the driver logs it.
5. Create I/O CQ 1 then I/O SQ 1 (bound to CQ 1), each with
   `min(32, MQES + 1)` entries.
6. Allocate an 8 KiB DMA buffer for data (two pages; allocations are
   4096-aligned, so the second page starts at IOVA + 4096).

## Transfers

- Per command at most `min(8 KiB, maximum transfer)` bytes, a whole number
  of blocks: 8 KiB because the shim allows at most two PRP entries. PRP1 =
  buffer IOVA; PRP2 = IOVA + 4096 if the command moves more than 4096
  bytes, else 0.
- `read(lba, count)` and `write(lba, data)` check `count > 0`, that the data
  is a whole number of blocks, and that `lba + count <= NSZE` (without
  overflowing), and return an error otherwise. **Every check happens
  before the first command is sent**, so a refused write writes nothing,
  not even the blocks that fit (the block error contract in
  `portals/wit`). They split the range into commands, issue them one at
  a time, wait for each completion (up to 500 ms), and fail on any non-zero
  status, naming SCT, SC and the LBA.
- `flush` issues Flush for namespace 1 and waits for it.

Every failure returns an error string that says what was attempted, what
was expected and what happened (rule 3).
