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
   - reads the first block, the last block and a 40-block run from LBA
     1000, and checks each against the seed pattern;
   - writes the **data pattern** to the same places, then calls `flush`;
   - reads them back and checks each against the data pattern.

   The run is 40 blocks (20 KiB): more than one command can carry
   (8 KiB, no PRP lists), so the driver must split it.
4. After the VM powers off, `image.py check` reads the image file: the 42
   written blocks must hold the data pattern and the other 32726 the seed.

Patterns, in 16-byte records: record `r` of block `lba` is the tag
(`SEED` or `DATA`), `lba` as a little-endian u64, then `r` as a
little-endian u32. No two blocks look alike, so a read from or write to
the wrong place cannot match. `image.py` and `block.rs` compute the
patterns separately.

**Pass** requires the VM checks to pass, `grind-host` to exit 0 (every
answer exact, no trap, timeout, driver error, denied write or refused
command) and the image check to pass.

Output: the host's JSON log (indented: `block-geometry`, `block-ok`,
`block-written`, `block-flushed`, or `block-mismatch` with the first bad
byte, its block, and the expected and actual 16-byte record),
`BLOCK step=<step> result=pass|fail expected="..." got="..."` for `host`
and `image`, and a final `BLOCK result=pass|fail`. Serial log:
`vm/build/serial.log`.

## Negative controls

Each must fail, which shows the test cannot pass without a working driver:

```sh
cargo build --release --manifest-path tests/block/modules/Cargo.toml --target wasm32-unknown-unknown
tests/block/check.sh tests/block/modules/target/wasm32-unknown-unknown/release/block_test_fake.wasm
tests/block/check.sh tests/block/modules/target/wasm32-unknown-unknown/release/block_test_cheat.wasm
```

| Module | What it does | How the test fails |
| --- | --- | --- |
| `fake` | never touches the device: right geometry, reads zeros, ignores writes | `block-mismatch` on the first seed read; image check: all 42 written blocks still hold the seed |
| `cheat` | never touches the device but knows the test: synthesizes the seed and keeps writes in memory | every check in the VM passes; **only the image check fails** (42 blocks: seed still there) |

`cheat` shows that the image check is independent of anything that ran
in the VM. `image.py check` was also run against a correctly written image
(pass) and one with a single stray byte in an untouched block (fail, naming
block 5000).

**Status:** both negative controls fail as above on QEMU 6.2.0. No driver
has passed yet: `drivers/nvme` is the next step.
