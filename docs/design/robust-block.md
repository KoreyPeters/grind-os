<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Design: robust block drivers, forced by the test

Status: **approved by the human on 2026-11-14**, with every recommendation (the power-loss phase uses (a), a write-back NBD disk).

## Why

CLAUDE.md rule 4 now says every device class's conformance test includes
that class's robustness contract, forced in every run. The NIC class has
one (`docs/design/robust-nic.md`); the block class does not. The
mutation score shows the gap: `tests/block` caught 3 of 8 mutants of the
NVMe driver, and the survivors were weakened validation (completion IDs,
the namespace format) that nothing in the test exercises.

The pattern is the NIC one: a contract in the portal docs, conditions
forced through code we control, a recovery check after each, a seeded
and recorded run. Storage needs different conditions.

## 1. The block robustness contract

Added to the `block` interface's docs in `portals/wit`; each block
driver's SPEC.md says how it meets each item.

| Condition | Required behaviour |
| --- | --- |
| The device reports an error for a command (a bad block) | That call returns a readable error naming the block range; other blocks keep working; no wedge. |
| A transient error (fails, then succeeds) | Retried a bounded number of times; the call succeeds if a retry does. |
| Nonsense in what the device returns (completion entries, Identify data) | Validated before use; a readable error, never a wrong answer, a trap or a hang. |
| A slow device | Every call still returns within the call limit or fails readably; nothing times out a command the device will still complete without accounting for it. |
| The device gone (registers all-ones, portal calls failing), then back | Calls return readable errors within the limit; the driver recovers without `start` being called again. |
| `flush` succeeded, then power is lost | Everything written before the flush is on the disk. Writes after it may or may not be, but no block holds anything but its old or its new contents. |
| DMA allocation fails | `start` returns a readable error. |
| Over a run | No trap, no timeout, no DMA growth after `start`, every error non-empty. |

**Not in the contract: silently wrong data.** A device that returns the
wrong bytes for a block with a success status cannot be detected without
checksums the device does not offer here, so the test never corrupts the
data itself, only what a driver can check (status, IDs, sizes, Identify).

## 2. Forcing the conditions

`tests/block` becomes a timeline, as `tests/nic` did. After each hostile
phase, a **recovery check**: a read and write of known blocks that must
succeed and match. The run's seed is random and printed for replay.

| Phase | How | Status |
| --- | --- | --- |
| baseline | today's exercise: geometry, seed reads, writes, flush, read-back, the error contract, the 256-block run | built |
| device errors | QEMU's `blkdebug` layer under the NVMe controller: reads and writes of a reserved range fail with a real I/O error (`errno 5`), so the controller returns genuine NVMe error completions; one block fails only its first two reads (transient) | checked: `blkdebug` in this QEMU fails exactly the configured sector |
| device nonsense | the host changes what the driver receives from the `nvme` portal: completion entries (status, command ID, SQ head) and the Identify data the device writes (the host knows the Identify buffer from the command) | new, in `host/src/faults.rs` |
| slow device | QEMU's `block_set_io_throttle` over QMP limits the disk to a trickle for a window | checked: the command exists in this QEMU |
| device gone | the NIC test's device-gone window: register reads all-ones, and the `nvme` portal returns `failed` for its length | extends `faults.rs` |
| allocation failure | `--fail-allocation 2`, as for NICs | built |
| power loss after flush | write, flush, write more without flushing, then the VM is killed abruptly; the image check then requires every pre-flush block to hold its new contents and every other written block to hold old or new | see the decision below |

**A limit of QEMU for the last phase.** QEMU writes guest data to the
image file as it goes, and the host's page cache keeps it even if QEMU
is killed, so killing the VM cannot make unflushed writes disappear. A
driver that never flushes would still pass. Two options:

- **(a) A write-back disk.** A small trusted program, std-only Rust,
  serves the image to QEMU over NBD and keeps unflushed writes in
  memory, writing them to the file only on flush. Killing it loses
  exactly what a real disk with a volatile cache would lose, so a missing
  or misplaced flush fails the test. About 300 lines of trusted code.
  **Recommended:** it is the only way the most important storage
  property gets tested at all.
- (b) Kill the VM anyway and check only that nothing is corrupted, and
  record that flush is untested in QEMU.

## 3. Measuring

Rerun `harness/mutants.sh nvme` before and after; the score should rise
from 3/8, and the survivors should be ones QEMU genuinely cannot tell
apart.

## 4. Then regrind

Run the NVMe driver against the new test. If it fails, as the NIC
drivers did, a fresh session regrinds it, recording each failing run.

## Decisions for the human

1. **The contract** in section 1, including leaving silently wrong data
   out of it.
2. **The power-loss phase:** (a) a write-back NBD disk (recommended) or
   (b) a corruption-only check with flush recorded as untested.
3. **Device nonsense through the `nvme` portal:** the host altering
   completion entries and Identify data as the driver receives them.
   As with the NIC faults, nothing that reaches the device changes, so
   rule 10 holds.
4. **Order of work**, as separate commits with negative controls:
   1. the contract and the timeline structure;
   2. `blkdebug` device errors;
   3. device nonsense and device-gone in the host;
   4. the throttle window;
   5. the power-loss phase;
   6. the mutation score, then regrinding the NVMe driver.

**Done 2026-11-14.** All six steps are in. The reground NVMe driver
passes; the mutation score rose from 3/8 to 9/20, and the survivors
change only logging, polling pauses or limits QEMU never reaches, apart
from two small gaps (the queue counts the device grants and the I/O
queue size).
