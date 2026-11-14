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

# The seed for the host's injected faults (tests/block/check.sh passes it
# on the kernel command line as grind.seed); 1 if none was given.
SEED=1
for arg in $(cat /proc/cmdline); do
	case "$arg" in grind.seed=*) SEED="${arg#grind.seed=}" ;; esac
done
echo "BLOCK fault-seed=$SEED"

# Allocation failure (robustness contract, docs/design/robust-block.md):
# the driver's second DMA allocation fails. `start` must then return a
# readable error, not trap or hang. Not exercised, and noted, if `start`
# allocates fewer than two buffers.
alloc_expected="with its 2nd DMA allocation failing, start returns a readable error (exit 1, driver-error)"
out0="$(/bin/grind-host --device "$BDF" --module "$MODULE" --timeout-ms 15000 \
	--exercise-block --expect-block-size "$BLOCK_SIZE" --expect-blocks "$BLOCKS" --fail-allocation 2 2>&1)"
rc0=$?
printf '%s\n' "$out0" | sed 's/^/    /'
last0="$(printf '%s\n' "$out0" | tail -n 1)"
if ! printf '%s' "$out0" | grep -qF '"kind":"allocation"'; then
	alloc=pass alloc_got="not exercised: start made fewer than 2 DMA allocations; exit $rc0"
elif [ "$rc0" -eq 1 ] && printf '%s' "$last0" | grep -qF '"reason":"driver-error"'; then
	alloc=pass alloc_got="exit 1; $(printf '%s\n' "$out0" | grep -F '"call":"start"' | head -n 1)"
else
	alloc=fail alloc_got="exit $rc0; last line: $last0"
fi
printf 'BLOCK step=alloc-fail result=%s expected="%s" got="%s"\n' "$alloc" "$alloc_expected" "$(clean "$alloc_got")"

# Nonsense in Identify (robustness contract): the namespace data reports a
# format index beyond the formats there are. `start` must refuse it with a
# readable error, not run with a nonsense block size.
id_expected="with impossible Identify Namespace data, start returns a readable error (exit 1, driver-error)"
out1="$(/bin/grind-host --device "$BDF" --module "$MODULE" --timeout-ms 15000 \
	--exercise-block --expect-block-size "$BLOCK_SIZE" --expect-blocks "$BLOCKS" --fault-identify 2>&1)"
rc1=$?
printf '%s\n' "$out1" | sed 's/^/    /'
last1="$(printf '%s\n' "$out1" | tail -n 1)"
if ! printf '%s' "$out1" | grep -qF '"kind":"identify-nonsense"'; then
	ident=fail ident_got="the nonsense was never read: no Identify Namespace data read from its buffer; exit $rc1"
elif [ "$rc1" -eq 1 ] && printf '%s' "$last1" | grep -qF '"reason":"driver-error"'; then
	ident=pass ident_got="exit 1; $(printf '%s\n' "$out1" | grep -F '"call":"start"' | head -n 1)"
else
	ident=fail ident_got="exit $rc1; last line: $last1"
fi
printf 'BLOCK step=identify-nonsense result=%s expected="%s" got="%s"\n' "$ident" "$id_expected" "$(clean "$ident_got")"

# No --allow-unreviewed-deny-list: the device's register and command lists
# are human-reviewed (rule 10), the same path real hardware takes.
# Streamed live, not collected and printed at the end: tests/block's
# director, outside the VM, acts on the host's phase markers as they appear
# on the serial console. awk flushes every line (sed would hold them back
# when writing to a pipe, and the director would act too late).
{
	/bin/grind-host --device "$BDF" --module "$MODULE" --timeout-ms 15000 \
		--exercise-block --expect-block-size "$BLOCK_SIZE" --expect-blocks "$BLOCKS" --fault-seed "$SEED" \
		--block-device-errors 2>&1
	echo "$?" >/tmp/host.rc
} | tee /tmp/host.out | awk '{ print "    " $0; fflush() }'
rc="$(cat /tmp/host.rc)"
out="$(cat /tmp/host.out)"
last="$(printf '%s\n' "$out" | tail -n 1)"
[ "$rc" -eq 0 ] && result=pass || result=fail
printf 'BLOCK step=host result=%s expected="%s" got="%s"\n' "$result" "$expected" "$(clean "exit $rc; last line: $last")"
[ "$alloc" = pass ] && [ "$ident" = pass ] || exit 1
exit "$rc"
