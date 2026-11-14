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
     phases are being added one at a time; today the recovery check runs
     once, after the baseline.

   Before this run, `guest.sh` runs the driver once with its second DMA
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

`cheat`, `lenient` and `partial` share `modules/cheat-common/cheat.rs`.
`cheat` shows that the image check is independent of anything that ran
in the VM; `lenient` and `partial` show that the error-contract checks
catch what they are for. `image.py check` was also run against a
correctly written image (pass), one with a single stray byte in an
untouched block (fail, naming block 5000) and one with `BADW` in block
32767 (fail, naming it).

**Status:** on QEMU 6.2.0, `drivers/nvme` passes and all four negative
controls fail as above.
