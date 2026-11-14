#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Runs inside the test VM (vm/run.sh --nvme; vm/init:
# grind.run=/tests/block/guest.sh). Runs the block driver under grind-host
# --exercise-block, which reads the seed, writes, flushes and reads back
# through the driver. tests/block/check.sh checks the disk image itself
# after the VM powers off.
#
# Prints the host's JSON log indented, then
#   BLOCK step=host result=pass|fail expected="..." got="..."
# Exits with grind-host's exit status.

BDF="$1"
MODULE=/drivers/driver.wasm
# The image tests/block/check.sh creates: 16 MiB of 512-byte blocks.
BLOCK_SIZE=512
BLOCKS=32768

clean() { printf '%s' "$*" | tr '\n"' "  "; }
expected="grind-host exits 0 after the block exercise on $BLOCKS blocks of $BLOCK_SIZE bytes"

if [ ! -f "$MODULE" ]; then
	printf 'BLOCK step=host result=fail expected="%s" got="%s"\n' "$expected" "$MODULE missing from the initramfs"
	exit 1
fi

# No --allow-unreviewed-deny-list: the device's register and command lists
# are human-reviewed (rule 10), the same path real hardware takes.
out="$(/bin/grind-host --device "$BDF" --module "$MODULE" --timeout-ms 15000 \
	--exercise-block --expect-block-size "$BLOCK_SIZE" --expect-blocks "$BLOCKS" 2>&1)"
rc=$?
printf '%s\n' "$out" | sed 's/^/    /'
last="$(printf '%s\n' "$out" | tail -n 1)"
[ "$rc" -eq 0 ] && result=pass || result=fail
printf 'BLOCK step=host result=%s expected="%s" got="%s"\n' "$result" "$expected" "$(clean "exit $rc; last line: $last")"
exit "$rc"
