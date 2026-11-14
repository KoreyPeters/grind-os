# SPDX-License-Identifier: AGPL-3.0-or-later
"""The block test's disk image: create it with the seed pattern, and check
it after the VM has powered off.

    image.py seed  IMAGE BLOCKS BLOCK_SIZE
    image.py check IMAGE BLOCKS BLOCK_SIZE [--no-power-loss]

Patterns, in 16-byte records: record r of block lba is the 4-byte tag
(b"SEED", b"DATA", b"RCVR" for the recovery checks' blocks, or b"BADW" for
writes that must fail), lba as a
little-endian u64, then r as a little-endian u32. This file computes them on its own, apart from grind-host's block.rs,
so the image check does not trust anything that ran in the VM.

`check` expects the data pattern in the blocks the host writes (the first
block, the last block, RUN_BLOCKS blocks from RUN_LBA and LONG_BLOCKS from
LONG_LBA) and the seed pattern in every other block. Writes the driver must
refuse (past the end, part of a block at block 5) would show up here as
damage to the last block or block 5. It prints
    BLOCK step=image result=pass|fail expected="..." got="..."
and exits 0 only on pass.
"""

import struct
import sys

RUN_LBA = 1000
RUN_BLOCKS = 40
LONG_LBA = 8192
LONG_BLOCKS = 256
# Recovery checks write the RCVR pattern here (host/src/block.rs).
RECOVERY_LBA = 2000
RECOVERY_BLOCKS = 8
# The power-loss phase: written and flushed before the cut (must be PWRA),
# and written after the flush (each block SEED or PWRB: a power cut may
# keep or lose an unflushed write, but never mangle it).
POWER_FLUSHED_LBA = 6000
POWER_UNFLUSHED_LBA = 6100
POWER_BLOCKS = 16


def block(tag, lba, size):
    return b"".join(tag + struct.pack("<QI", lba, r) for r in range(size // 16))


def recovery():
    return set(range(RECOVERY_LBA, RECOVERY_LBA + RECOVERY_BLOCKS))


def written(blocks):
    return ({0, blocks - 1} | set(range(RUN_LBA, RUN_LBA + RUN_BLOCKS))
            | set(range(LONG_LBA, LONG_LBA + LONG_BLOCKS)))


def seed(path, blocks, size):
    with open(path, "wb") as f:
        for lba in range(blocks):
            f.write(block(b"SEED", lba, size))


def check(path, blocks, size, power_loss=True):
    data = written(blocks)
    rcvr = recovery()
    # --no-power-loss: the run never reached the power cut, so those blocks
    # were never written and must still hold the seed.
    flushed = set(range(POWER_FLUSHED_LBA, POWER_FLUSHED_LBA + POWER_BLOCKS)) if power_loss else set()
    unflushed = set(range(POWER_UNFLUSHED_LBA, POWER_UNFLUSHED_LBA + POWER_BLOCKS)) if power_loss else set()
    kept_unflushed = 0
    expected = (
        f"{len(data)} written blocks hold the DATA pattern (0, {blocks - 1}, "
        f"{RUN_LBA}-{RUN_LBA + RUN_BLOCKS - 1}, {LONG_LBA}-{LONG_LBA + LONG_BLOCKS - 1}); "
        f"{len(rcvr)} recovery blocks ({RECOVERY_LBA}-{RECOVERY_LBA + RECOVERY_BLOCKS - 1}) hold RCVR; "
        + (f"{POWER_BLOCKS} blocks flushed before the power cut ({POWER_FLUSHED_LBA}-) hold PWRA; "
           f"{POWER_BLOCKS} written after it ({POWER_UNFLUSHED_LBA}-) hold SEED or PWRB; "
           if power_loss else "no power cut (the run ended before it); ")
        + f"the other {blocks - len(data) - len(rcvr) - len(flushed) - len(unflushed)} hold the SEED pattern"
    )
    bad = []
    with open(path, "rb") as f:
        image = f.read()
    if len(image) != blocks * size:
        got = f"image is {len(image)} bytes, expected {blocks * size}"
    else:
        for lba in range(blocks):
            have = image[lba * size:(lba + 1) * size]
            if lba in unflushed:
                if have == block(b"PWRB", lba, size):
                    kept_unflushed += 1
                    continue
                if have == block(b"SEED", lba, size):
                    continue
                bad.append(f"block {lba} (written after the flush): neither its old nor its new contents, first record {have[:16].hex()}")
                continue
            tag = b"RCVR" if lba in rcvr else b"DATA" if lba in data else b"PWRA" if lba in flushed else b"SEED"
            if have != block(tag, lba, size):
                what = "seed still there" if have == block(b"SEED", lba, size) else \
                    "data pattern" if have == block(b"DATA", lba, size) else \
                    "BADW: a write that had to fail reached the disk" if have[:4] == b"BADW" else \
                    f"other bytes, first record {have[:16].hex()}"
                role = 'recovery' if lba in rcvr else 'written' if lba in data else \
                    'flushed before the power cut' if lba in flushed else 'untouched'
                bad.append(f"block {lba} ({role}): {what}")
        got = ((f"all blocks as expected ({kept_unflushed} of {POWER_BLOCKS} unflushed blocks survived the power cut)"
                if power_loss else "all blocks as expected") if not bad else None)
        got = got or \
            f"{len(bad)} blocks wrong; first: " + "; ".join(bad[:5])
    result = "pass" if not bad and len(image) == blocks * size else "fail"
    print(f'BLOCK step=image result={result} expected="{expected}" got="{got}"')
    return result == "pass"


def main():
    no_power_loss = "--no-power-loss" in sys.argv
    argv = [a for a in sys.argv if a != "--no-power-loss"]
    if len(argv) != 5 or argv[1] not in ("seed", "check"):
        print(__doc__, file=sys.stderr)
        return 2
    path, blocks, size = argv[2], int(argv[3]), int(argv[4])
    if argv[1] == "seed":
        seed(path, blocks, size)
        return 0
    return 0 if check(path, blocks, size, power_loss=not no_power_loss) else 1


if __name__ == "__main__":
    sys.exit(main())
