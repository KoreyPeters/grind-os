<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Provenance: nvme driver

## Sources consulted

All kept in the gitignored `.cache/sources/`; fetched from GitHub raw
URLs at the exact commits.

- **FreeBSD `sys/dev/nvme/`**, `freebsd/freebsd-src` commit
  `74d72b3491e810068313e49629c3b8fe46f29318`, BSD-2-Clause:
  - `nvme.h`, sha256 `e9ee9a96fae349db7b78b8b91455b94c538f2f1080554fab6711269b86e6d036`:
    register, command, completion, Identify Controller and Identify
    Namespace layouts; opcodes; feature IDs; status fields; the format
    index helper.
  - `nvme_ctrlr.c`, sha256 `d54ff501a0a5f8993ed518f7d9625a149ffdecf1b71cc73e0342e3d6ab7dbd40`:
    the maximum transfer size from MDTS (lines 535-537).
  - `nvme_ctrlr_cmd.c`, sha256 `8287a9d13f5dff894c1c4ef7ae5915d11c7d3901b6fbb650d485d11627467c43`:
    how Identify, Set Features (Number of Queues) and queue creation are
    filled in.
  - `nvme_qpair.c`, sha256 `44990e079e6fe234c068c2f1799fbbdc5406f591bb055a08a976440cd2413491`:
    only read; PRP usage (the shim's rules already cover it).
- **SPDK `include/spdk/nvme_spec.h`**, `spdk/spdk` commit
  `91404c6437e45395a943bfeaf48f58ea1d6aff74`, BSD-3-Clause, sha256
  `244249b1d9b560484851b804de8155f371de4e5d80e5f9719b2db5006a3ef01c`:
  field positions in CDW10-12 for queue creation and Read/Write (lines
  1203-1205, 1308-1323, 1386-1391).
- **This repository's `shim/README.md`, `portals/wit/portals.wit` and
  `docs/design/milestone-5-nvme.md`**: what the shim does and allows.

## Not consulted

- The NVM Express specification: its notice gives non-members only the
  right to review and cite it (decision recorded in
  `docs/design/milestone-5-nvme.md`).
- Linux's `nvme` driver (GPL-2.0-only, rule 6).
- QEMU's `hw/nvme/` device model. Behaviour observed while running is
  recorded below as observations.

## How they were used

`SPEC.md` was written from the sources above; the driver is written from
`SPEC.md`. Offsets, bit positions and command layouts are facts; no code
was copied, adapted or translated.

## Observations of device behaviour

- QEMU 6.2.0 (from the shim's `nvme-ready` log): CAP.DSTRD = 0, MQES + 1
  = 2048, serial number `grind0001` (set by `vm/run.sh`), namespace 1
  with 512-byte blocks.
- QEMU 6.2.0, running this driver: Identify Controller reports MDTS 7
  (512 KiB), so the shim's two-PRP limit (8 KiB) is the one that applies.
  Set Features (Number of Queues) asked for one SQ and one CQ and reported
  64 of each granted (QEMU's `max_ioqpairs`); the driver uses one. The
  first grind passed `tests/block` (harness run `20261114T151952Z`).

## Independent rewrite (2026-11-14)

To check that `SPEC.md` is enough on its own, a fresh Claude session with
none of this session's context wrote a second driver in a separate
worktree. It was given only `CLAUDE.md`, this directory's `SPEC.md` and
`PROVENANCE.md`, the portal WIT, the trusted code and READMEs, the
FreeBSD/SPDK sources and `drivers/e1000e`. It was barred from the
existing driver source (including git history), session transcripts, the
NVMe specification, Linux and the web, and reported keeping to that. Its
508-line driver passed `harness/grind.sh nvme`, including the block error
contract, on the first harness run, without changing `SPEC.md`. It was
not kept: it is equivalent to this one.

It found four gaps in `SPEC.md`, none of which caused a failure, and
they are now filled: checking admin completions' CID and status, not
only I/O ones; failing at start on a block size outside 512–4096 bytes;
doing every argument check before the first command, so a refused write
writes nothing; and what happens to the Identify buffer. This driver
already did all four except the admin CID check, which it now does.

Neither driver ever failed a harness run, so this exercised the spec,
not the grind loop of fixing a driver from readable failures.
