#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Block conformance (milestone 5): block size and capacity, exact reads and
# writes of the first block, the last block and a multi-block run, flush.
#
#   tests/block/check.sh <driver.wasm> [vm/run.sh options]
#
#   1. Build grind-host (static).
#   2. Create a 16 MiB scratch image holding the seed pattern (image.py).
#   3. Boot the test VM with an NVMe controller backed by the image and run
#      tests/block/guest.sh: grind-host --exercise-block through the driver.
#   4. After the VM powers off, check the image file itself: the written
#      blocks hold the data pattern and every other block the seed.
#   5. Pass only if both the VM run and the image check pass.
#
# Prints BLOCK step=... lines and ends with "BLOCK result=pass|fail".
# Requires vm/fetch.sh to have run once.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET=x86_64-unknown-linux-gnu
HOST_BIN="$ROOT/target/$TARGET/release/grind-host"
WORK="$ROOT/target/block-test"
IMAGE="$WORK/disk.img"
BLOCK_SIZE=512
BLOCKS=32768

fail() { # step expected got
	printf 'BLOCK step=%s result=fail expected="%s" got="%s"\n' "$1" "$2" "$3"
	echo "BLOCK result=fail"
	exit 1
}

[ $# -ge 1 ] || fail args "tests/block/check.sh <driver.wasm> [vm/run.sh options]" "no driver given"
DRIVER="$(realpath "$1")"
shift
[ -f "$DRIVER" ] || fail args "a driver module at $DRIVER" "missing"

RUSTFLAGS="-C target-feature=+crt-static" \
	cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" -p grind-host --target "$TARGET" ||
	fail build-host "cargo build -p grind-host succeeds" "exit $?"

mkdir -p "$WORK"
rm -f "$IMAGE"
python3 -I "$ROOT/tests/block/image.py" seed "$IMAGE" "$BLOCKS" "$BLOCK_SIZE" ||
	fail image-seed "image.py seed writes $IMAGE" "exit $?"

"$ROOT/vm/build-initramfs.sh" \
	--add "$HOST_BIN=/bin/grind-host" \
	--add "$ROOT/tests/block/guest.sh=/tests/block/guest.sh" \
	--add "$DRIVER=/drivers/driver.wasm"

set +e
"$ROOT/vm/run.sh" --check --nvme "$IMAGE" --run /tests/block/guest.sh --timeout 180 "$@"
vm_status=$?
echo
python3 -I "$ROOT/tests/block/image.py" check "$IMAGE" "$BLOCKS" "$BLOCK_SIZE"
image_status=$?
set -e

if [ "$vm_status" -eq 0 ] && [ "$image_status" -eq 0 ]; then
	echo "BLOCK result=pass"
	exit 0
fi
printf 'BLOCK step=summary result=fail expected="%s" got="%s"\n' \
	"VM run exits 0 and the image check passes" "vm exit $vm_status, image check exit $image_status"
echo "BLOCK result=fail"
exit 1
