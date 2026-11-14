<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Block conformance (milestone 5): read, write and flush exactly

```sh
vm/fetch.sh                                # once
tests/block/check.sh <driver.wasm>         # exit 0 only if everything passes
```

The oracle is the disk image itself, checked outside the VM after it powers
off, as `nic-peer` is for the NIC test.

1. `check.sh` builds `grind-host` (static) and creates a 16 MiB scratch
   image, `target/block-test/disk.img`, holding the **seed pattern**
   (`image.py seed`).
2. It boots the VM with `vm/run.sh --nvme` on that image. In the VM,
   `guest.sh` runs the driver (world `block-driver`) under
   `grind-host --exercise-block --expect-block-size 512 --expect-blocks 32768`,
   without `--allow-unreviewed-deny-list`.
3. After `start`, the host (`host/src/block.rs`):
   - checks `block-size` and `capacity` against 32768 blocks of 512 bytes;
   - reads the first block, the last block, a 40-block run from LBA 1000
     and a 256-block run from LBA 8192, and checks each against the seed
     pattern;
   - writes the **data pattern** to the same places, then calls `flush`;
   - reads them back and checks each against the data pattern;
   - checks the **error contract** (`portals/wit`, interface `block`):
     each call below must return a non-empty error;

     | Call | Why it must fail |
     | --- | --- |
     | `read(32768, 1)` | starts at capacity |
     | `read(32767, 2)` | runs past the end |
     | `read(u64::MAX, 1)` | `lba + count` overflows |
     | `read(0, 0)` | no blocks |
     | `write(32767, 2 blocks of BADW)` | runs past the end; must write **nothing**, not even block 32767 |
     | `write(5, 100 bytes)` | not a whole block |
     | `write(0, nothing)` | no blocks |

   - reads the last block and the first block again: both must still hold
     the data pattern, so the driver kept working and the refused write
     left block 32767 alone.

   The runs are more than one command can carry (8 KiB, no PRP lists): the
   40-block run needs 3 commands, the 256-block run 16.
   - **robustness** (`docs/design/robust-block.md`): hostile phases
     follow, each ended by a **recovery check**: write the `RCVR`
     pattern to blocks 2000-2007, flush, and read it back exactly
     (`block-recovery`). A driver error during a hostile phase is
     logged, not fatal; a failed recovery check fails the run. The
     phases are being added one at a time:
     - **completion nonsense**: for 1.5 s the host changes one byte of
       15% of the completion entries the driver receives (result, SQ
       head, command ID or status; `host/src/faults.rs`) while it reads
       known seed blocks (5000-5063). Reads may fail readably, never
       return wrong data (`block-wrong-answer`).
     - **device gone**: for 1.5 s register reads return all-ones and
       every `nvme` portal call fails; reads must fail readably within
       the limit, and the recovery check afterwards must pass without
       `start` being called again.
     - **slow device**: the host prints a phase marker; `director.py`,
       outside the VM, follows the serial log and throttles the disk to
       1 I/O per second through QEMU's monitor (`block_set_io_throttle`),
       then lifts it at the end marker. Every call must still return
       within the limit; reads may fail readably, never return wrong
       data; and a command the driver gave up on but the device
       completes later must not confuse it (the recovery check after the
       phase). `guest.sh` streams the host's output live, flushing every
       line, so the director acts on time.
     - **power loss after flush** (always last): the host writes blocks
       6000-6015 (`PWRA`) and flushes, writes 6100-6115 (`PWRB`) without
       flushing, prints a marker and waits; the director cuts the power.
       The disk is served by `wbdisk/` (a write-back disk: QEMU's NVMe
       storage comes over NBD, `vm/run.sh --nvme-nbd`, and writes stay in
       wbdisk's memory until a flush), so killing wbdisk loses exactly
       what a real disk's volatile cache would. The image check then
       requires `PWRA` in all of 6000-6015, and in 6100-6115 each block's
       old (`SEED`) or new (`PWRB`) contents, nothing else.
       Since the run ends with the cut, the VM never reports a result:
       `check.sh` judges from the serial log (both pre-runs passed, the
       host reached the power-loss marker without failing, the director
       cut the power) and from the image. If the run fails before the cut,
       the power-loss blocks are not expected (`image.py --no-power-loss`).
     - **device errors**: QEMU's `blkdebug` layer under the disk
       (`vm/run.sh --nvme-errors`, config written by `check.sh`) fails
       reads of block 3000 and writes to block 3100 with real I/O
       errors, and the first read of block 3200 only. The controller
       returns genuine NVMe error completions (SCT 2, SC 0x81 or 0x80,
       Do Not Retry clear). Reads of and across block 3000 and the write
       to 3100 must give readable errors; the read of 3200 must succeed
       after a bounded retry (`block-transient-not-retried` otherwise);
       the neighbouring blocks must keep working.

   Before this run, `guest.sh` also runs the driver with impossible
   Identify Namespace data (`--fault-identify`: a format index beyond the
   formats there are): `start` must refuse it readably
   (`identify-nonsense`). A copy of the NVMe driver with its format
   checks removed accepted it and failed with `block-geometry` instead.

   Also before this run, `guest.sh` runs the driver once with its second DMA
   allocation failing (`--fail-allocation 2`): `start` must return a
   readable error, not trap or hang (`alloc-fail`). The run's fault seed
   is random, printed as `BLOCK fault-seed=N (replay with
   BLOCK_FAULT_SEED=N)`, and passed to the host.
4. After the VM powers off, `image.py check` reads the image file: the 298
   written blocks must hold the data pattern and the other 32470 the seed.
   A refused write that reached the disk anyway shows up as `BADW` in
   block 32767 or damage to block 5, named by block number.

Patterns, in 16-byte records: record `r` of block `lba` is the tag
(`SEED`, `DATA`, or `BADW` for writes that must fail), `lba` as a little-endian u64, then `r` as a
little-endian u32. No two blocks look alike, so a read from or write to
the wrong place cannot match. `image.py` and `block.rs` compute the
patterns separately.

**Pass** requires the VM checks to pass, `grind-host` to exit 0 (every
answer exact, no trap, timeout, driver error, denied write or refused
command) and the image check to pass.

Output: the host's JSON log (indented: `block-geometry`, `block-ok`,
`block-written`, `block-flushed`, `block-error-ok`, or a failure:
`block-mismatch` with the first bad byte, its block, and the expected and
actual 16-byte record; `block-accepted-invalid` for a call that should
have failed; `block-error-unreadable` for an empty error),
`BLOCK step=<step> result=pass|fail expected="..." got="..."` for `host`
and `image`, and a final `BLOCK result=pass|fail`. Serial log:
`vm/build/serial.log`.

## Negative controls

Each must fail, which shows the test cannot pass without a working driver:

```sh
cargo build --release --manifest-path tests/block/modules/Cargo.toml --target wasm32-unknown-unknown
tests/block/check.sh tests/block/modules/target/wasm32-unknown-unknown/release/block_test_fake.wasm
tests/block/check.sh tests/block/modules/target/wasm32-unknown-unknown/release/block_test_cheat.wasm
tests/block/check.sh tests/block/modules/target/wasm32-unknown-unknown/release/block_test_lenient.wasm
tests/block/check.sh tests/block/modules/target/wasm32-unknown-unknown/release/block_test_partial.wasm
```

| Module | What it does | How the test fails |
| --- | --- | --- |
| `fake` | never touches the device: right geometry, reads zeros, ignores writes | `block-mismatch` on the first seed read; image check: all 298 written blocks still hold the seed |
| `cheat` | never touches the device but knows the test: synthesizes the seed, keeps writes in memory, follows the error contract | every check in the VM passes; **only the image check fails** (298 blocks: seed still there) |
| `lenient` | `cheat`, but reads outside the disk return data | `block-accepted-invalid` at `read(32768, 1)` |
| `partial` | `cheat`, but a write past the end keeps the block that fits before failing | `block-mismatch` at the re-read of block 32767: `BADW` instead of `DATA` |
| `alloc-panic` | unwraps a failed DMA allocation in `start` | `alloc-fail`: a trap, not a readable error |

Power loss was checked with the NVMe driver (with the device-error
phases switched off, since it fails them; see Status): all 16 flushed
blocks held `PWRA`, and 0 of 16 unflushed blocks survived the cut, as a
volatile cache should lose them. A copy whose `flush` returns ok without
sending the NVMe Flush left the disk with 0 flushes: 322 blocks wrong
after the cut. image.py was also checked offline: 5 of 16 unflushed
blocks kept passes; one byte changed in an unflushed block fails.

`cheat` also pretends blocks 3000 and 3100 are bad, so the
device-errors phase cannot catch it either: it stays the control that
only the image check can catch.

**Status (2026-11-14):** the NVMe driver refuses impossible Identify
data, survives completion nonsense (937 corrupted completions, no wrong
answers) and the device going away, and gives readable errors for the
bad blocks and for reads of the throttled disk. It fails two items:
it does not retry the transient error, and after the slow-device phase
the completions of commands it gave up on arrive late and it rejects
the next command ("expected one 16-byte completion, got 4") instead of
discarding them
(`block-transient-not-retried`). It will be reground against the full
test (docs/design/robust-block.md, step 6).

`cheat`, `lenient` and `partial` share `modules/cheat-common/cheat.rs`.
`cheat` shows that the image check is independent of anything that ran
in the VM; `lenient` and `partial` show that the error-contract checks
catch what they are for. `image.py check` was also run against a
correctly written image (pass), one with a single stray byte in an
untouched block (fail, naming block 5000) and one with `BADW` in block
32767 (fail, naming it).

**Status:** on QEMU 6.2.0, `drivers/nvme` passes and all four negative
controls fail as above.
