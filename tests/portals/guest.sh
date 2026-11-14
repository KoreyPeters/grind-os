#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Runs inside the test VM (vm/init: grind.run=/tests/portals/guest.sh).
# Each case runs grind-host with one test module and checks its exit status
# and one expected log line. Cases are independent, so all of them run.
#
# Prints, per case:
#   PORTALS step=<case> result=pass|fail expected="..." got="..."
# and the host's JSON log indented underneath. Exits 0 only if all pass.

BDF="$1"
HOST=/bin/grind-host
DIR=/tests/portals
failed=0

clean() { printf '%s' "$*" | tr '\n"' "  "; }

# run_case <name> <expected-exit> <expected-substring> <module> [host args...]
run_case() {
	name="$1" want_rc="$2" want="$3" module="$4"
	shift 4
	expected="exit $want_rc and a log line containing $want"
	if [ ! -f "$DIR/$module.wasm" ]; then
		printf 'PORTALS step=%s result=fail expected="%s" got="%s"\n' "$name" \
			"$(clean "$expected")" "$(clean "$DIR/$module.wasm missing from the initramfs")"
		failed=1
		return
	fi
	out="$("$HOST" --device "$BDF" --module "$DIR/$module.wasm" "$@" 2>&1)"
	rc=$?
	printf '%s\n' "$out" | sed 's/^/    /'
	last="$(printf '%s\n' "$out" | tail -n 1)"
	if [ "$rc" -eq "$want_rc" ] && printf '%s' "$out" | grep -qF -- "$want"; then
		result=pass
	else
		result=fail
		failed=1
	fi
	printf 'PORTALS step=%s result=%s expected="%s" got="%s"\n' "$name" "$result" \
		"$(clean "$expected")" "$(clean "exit $rc; last line: $last")"
}

# The e1000e deny list is unreviewed (rule 10), so cases that open the
# device must allow that explicitly. This is valid only for emulated devices.
EMU=--allow-unreviewed-deny-list

run_case read-status 0 '"message":"STATUS","field.value":"0x' read-status $EMU
run_case bounds 0 '"attempted":"read32","offset":"0x2","error":"misaligned"' bounds $EMU
run_case dma 0 '"status":"pass"' dma $EMU
run_case wasi-rejected 2 'import wasi_snapshot_preview1::proc_exit is not a portal' wasi-import $EMU
run_case timeout 1 '"reason":"timeout"' spin $EMU --timeout-ms 1000
run_case trap 1 '"reason":"trap"' trap $EMU

# A sleep may not carry a call past its deadline (issue #10): the module
# sleeps 1 s at a time under a 300 ms limit, and must be stopped as a
# timeout well before its first sleep would have ended. Elapsed time is the
# host's whole run, including opening the device.
sl_expected="exit 1, reason timeout, and the run over within 600 ms (limit 300 ms; one sleep is 1000 ms)"
if [ ! -f "$DIR/sleep.wasm" ]; then
	sl=fail sl_got="$DIR/sleep.wasm missing from the initramfs"
else
	out="$("$HOST" --device "$BDF" --module "$DIR/sleep.wasm" $EMU --timeout-ms 300 2>&1)"
	rc=$?
	printf '%s\n' "$out" | sed 's/^/    /'
	ms="$(printf '%s\n' "$out" | grep -o '"elapsed_ms":[0-9]*' | tail -n 1 | cut -d: -f2)"
	sl_got="exit $rc; elapsed_ms ${ms:-none}"
	if [ "$rc" -eq 1 ] && printf '%s' "$out" | grep -qF '"reason":"timeout"' && [ -n "$ms" ] && [ "$ms" -le 600 ]; then
		sl=pass
	else
		sl=fail
	fi
fi
[ "$sl" = pass ] || failed=1
printf 'PORTALS step=sleep-deadline result=%s expected="%s" got="%s"\n' "$sl" "$sl_expected" "$(clean "$sl_got")"

# deny-list-gate: without --allow-unreviewed-deny-list, the host must follow
# the list's reviewed flag: refuse at device-deny-list while the list is
# unreviewed, and open the device (deny_reviewed true) once the human has
# reviewed it (rule 10). The gate logic itself is unit-tested in the shim
# (deny.rs) with test-only lists in both states.
expected="unreviewed list: exit 2 at device-deny-list; reviewed list: exit 0 with deny_reviewed true"
out="$("$HOST" --device "$BDF" --module "$DIR/read-status.wasm" 2>&1)"
rc=$?
printf '%s\n' "$out" | sed 's/^/    /'
if [ "$rc" -eq 2 ] && printf '%s' "$out" | grep -qF '"step":"device-deny-list"' &&
	printf '%s' "$out" | grep -qF 'not reviewed yet'; then
	result=pass got="exit 2; refused, list not reviewed"
elif [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -qF '"deny_reviewed":true'; then
	result=pass got="exit 0; opened, list reviewed"
else
	result=fail got="exit $rc; last line: $(printf '%s\n' "$out" | tail -n 1)"
	failed=1
fi
printf 'PORTALS step=deny-list-gate result=%s expected="%s" got="%s"\n' "$result" \
	"$(clean "$expected")" "$(clean "$got")"

# The shim reads the DMI system vendor that gates --allow-unreviewed-deny-list
# (rule 10). In the test VM it must be QEMU. The refusal on real hardware is
# unit-tested in shim/src/deny.rs; it cannot be reached here because the VM
# is an emulator and its only device has a reviewed list.
run_case emulator-detected 0 '"dmi_sys_vendor":"QEMU"' read-status $EMU
run_case denied-write 1 '"event":"mmio-denied","attempted":"write32","offset":"0x102c"' denied-write $EMU

# The shim resets the device at open and clears bus mastering before the
# driver's DMA buffers are unmapped (review finding #2).
run_case reset-at-open 0 '"reset":"VFIO_DEVICE_RESET ok"' read-status $EMU
run_case quiesce-at-close 0 '"event":"device-quiesce","bdf":"'"$BDF"'","command_before":"0x' dma $EMU

# dma.allocate32 (issue #13): buffers below 4 GiB, from 1 MiB, clear of the
# MSI window, with guard pages; the shim logs the IOMMU's usable ranges.
run_case dma32 0 '"message":"dma32","field.first":"0x100000"' dma32 $EMU
run_case dma-regions 0 '"event":"dma-regions","iova_ranges":"0x0-0xfedfffff 0xfef00000-0x7fffffffff","high":"0x100000000-0x8000000000","low":"0x100000-0xfee00000"' dma32 $EMU

# The nvme portal exists only for a device that takes commands: a module
# importing it is rejected on the e1000e before it runs.
run_case nvme-portal-on-nic 2 'grind:portals/nvme@0.1.0 only for a device that takes NVMe commands' nvme-format $EMU
# ...and the result line is still the last line: the device is closed
# before the setup error is reported.
last="$(printf '%s\n' "$out" | tail -n 1)"
if printf '%s' "$last" | grep -qF '"event":"result"'; then result=pass; else result=fail failed=1; fi
printf 'PORTALS step=result-last result=%s expected="%s" got="%s"\n' "$result" \
	"the result line last after a setup error" "$(clean "$last")"

# After every case above: the IOMMU logged no DMA faults (teardown included).
expected="no DMAR fault lines in dmesg after all cases"
faults="$(dmesg | grep -E 'DMAR: \[DMA (Read|Write)|DMAR: DRHD: handling fault' || true)"
if [ -z "$faults" ]; then
	result=pass got="none"
else
	result=fail got="$(printf '%s\n' "$faults" | head -n 5)"
	failed=1
	printf '%s\n' "$faults" | sed 's/^/    /'
fi
printf 'PORTALS step=no-dma-faults result=%s expected="%s" got="%s"\n' "$result" "$expected" "$(clean "$got")"

exit "$failed"
