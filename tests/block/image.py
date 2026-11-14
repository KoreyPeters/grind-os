# SPDX-License-Identifier: AGPL-3.0-or-later
"""The block test's disk image: create it with the seed pattern, and check
it after the VM has powered off.

    image.py seed  IMAGE BLOCKS BLOCK_SIZE
    image.py check IMAGE BLOCKS BLOCK_SIZE

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


def check(path, blocks, size):
    data = written(blocks)
    rcvr = recovery()
    expected = (
        f"{len(data)} written blocks hold the DATA pattern (0, {blocks - 1}, "
        f"{RUN_LBA}-{RUN_LBA + RUN_BLOCKS - 1}, {LONG_LBA}-{LONG_LBA + LONG_BLOCKS - 1}); "
        f"{len(rcvr)} recovery blocks ({RECOVERY_LBA}-{RECOVERY_LBA + RECOVERY_BLOCKS - 1}) hold RCVR; "
        f"the other {blocks - len(data) - len(rcvr)} hold the SEED pattern"
    )
    bad = []
    with open(path, "rb") as f:
        image = f.read()
    if len(image) != blocks * size:
        got = f"image is {len(image)} bytes, expected {blocks * size}"
    else:
        for lba in range(blocks):
            tag = b"RCVR" if lba in rcvr else b"DATA" if lba in data else b"SEED"
            have = image[lba * size:(lba + 1) * size]
            if have != block(tag, lba, size):
                what = "seed still there" if have == block(b"SEED", lba, size) else \
                    "data pattern" if have == block(b"DATA", lba, size) else \
                    "BADW: a write that had to fail reached the disk" if have[:4] == b"BADW" else \
                    f"other bytes, first record {have[:16].hex()}"
                role = 'recovery' if lba in rcvr else 'written' if lba in data else 'untouched'
                bad.append(f"block {lba} ({role}): {what}")
        got = "all blocks as expected" if not bad else \
            f"{len(bad)} blocks wrong; first: " + "; ".join(bad[:5])
    result = "pass" if not bad and len(image) == blocks * size else "fail"
    print(f'BLOCK step=image result={result} expected="{expected}" got="{got}"')
    return result == "pass"


def main():
    if len(sys.argv) != 5 or sys.argv[1] not in ("seed", "check"):
        print(__doc__, file=sys.stderr)
        return 2
    path, blocks, size = sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    if sys.argv[1] == "seed":
        seed(path, blocks, size)
        return 0
    return 0 if check(path, blocks, size) else 1


if __name__ == "__main__":
    sys.exit(main())
