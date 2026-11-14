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
    PRP usage (the shim's rules already cover it); for the robustness
    regrind, facts only: FreeBSD's retry policy and its bounded retry count
    (lines 150-225), and matching each completion to one outstanding
    command by CID (lines 436-461).
  - `nvme_private.h`: read only (timeouts, lines 82-87).
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

## Robustness grind log (2026-11-14)

Regrinding against the strengthened `tests/block` (the block robustness
contract). Sources: only those above, plus observed device behaviour.
Local builds are `cargo build --release --target wasm32-unknown-unknown`
in `drivers/nvme/`, to check that the code compiles; no local tests.

1. **Run `harness/runs/nvme/20261114T195309Z`**, seed 4084995691, **fail**
   (unchanged driver, to confirm the known gaps). Failing step: recovery
   check after `slow-device`: `write: Write LBA 2000: expected one 16-byte
   completion, got 4 (16 bytes)`. The slow phase gave 1 read ok, 7
   failed (500 ms wait). Diagnosis: the driver gave up on reads after
   500 ms but the device still completed them; their entries arrived
   together with the next command's. Worse than the message says: the
   given-up reads would also have DMA'd into the shared data buffer
   after it was reused. The device-errors phase was not reached, so the
   transient-retry gap was not seen in this run (known from
   `tests/block/README.md`). Change: rewrote the I/O path: at most one
   command outstanding including given-up ones (the next call first waits
   for and discards its completion, counted by entries, not their
   contents); a per-call 750 ms budget; bounded retry (4 attempts) on an
   error status with DNR clear or a completion that cannot be the
   command's; errors name the LBA range and the call; portal errors
   include CSTS. 1 local build.
2. **Run `20261114T195655Z`**, seed 3688991623, **fail**. Every phase in
   the VM passed (recovery after all phases, transient read retried to
   success, power-loss image check passed), but the summary failed:
   `BLOCK step=alloc-fail result=fail expected="the alloc-fail pre-run
   passes" got="BLOCK step=alloc-fail result=pass ..."` (same for
   identify-nonsense), and the slow phase read 7150 blocks ok with the
   slowest call 2 ms. Diagnosis: the driver logged every retry (about
   380 lines), the serial log grew to 161 KB, and (a) the director, which
   follows the serial log, saw the slow-device start and end markers at
   the same moment (`director.log`: throttled at t=69383.632, lifted at
   .633), after the phase was over, so the device was never slowed; (b)
   `check.sh` judges the pre-runs with `log | grep -aqE ...` under
   `set -o pipefail`; on a large log `grep -q` exits at the early match,
   `tr` dies of SIGPIPE, and the pipeline reports failure although the
   line is there (the `got` shows it). Both are trusted-code fragilities,
   reported, not patched. Change: recoverable events (retries, late
   completions) are logged in full for the first three of each kind, then
   summarised with a count and the latest event at most once a second.
   1 local build.
3. **Run `20261114T195812Z`**, seed 3943248975, **pass**. Serial log
   82 KB (16 driver lines; 559 are the host's fault-injection lines).
   Slow phase now really throttled: 1 read ok, 4 failed readably, slowest
   call 750 ms; three late completions discarded about 1 s after
   submission; recovery passed. Completion nonsense: 3204 reads ok, 23
   failed readably, about 200 succeeded after a retry. Transient block
   3200 succeeded on attempt 2. Then `SPEC.md` gained the robustness
   section (no code change).
4. **Run `20261114T195932Z`**, seed 921261360, **pass** (confirmation;
   driver as in run 3, `SPEC.md` and this log updated). Slow phase
   throttled: 1 read ok, 4 failed readably, slowest call 750 ms.
   0 local builds since run 3.
5. **Run `20261114T195954Z`**, seed 1624388308, **pass** (confirmation,
   no change). The director lifted the throttle about 2.3 s after setting
   it, so most of the 3 s slow window ran unthrottled (3033 reads ok, 3
   failed readably at 750 ms): the serial log (88 KB, mostly the host's
   fault-injection lines) still lags the host. 0 local builds.

In all: 5 harness runs (2 failing, 3 passing), 2 local builds.
