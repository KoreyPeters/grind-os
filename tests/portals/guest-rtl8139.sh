#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Runs inside the test VM booted with vm/run.sh --nic rtl8139 (vm/init:
# grind.run=/tests/portals/guest-rtl8139.sh), against QEMU's RTL8139. Checks
# the register window (BAR1, not the I/O-port BAR0), 8- and 16-bit mmio
# accesses, the deny list (rule 10), reset at open and quiesce. Each case
# runs grind-host with one test module and checks its exit status and every
# expected log fragment.
#
# Prints, per case:
#   PORTALS step=<case> result=pass|fail expected="..." got="..."
# and the host's JSON log indented underneath. Exits 0 only if all pass.

BDF="$1"
HOST=/bin/grind-host
DIR=/tests/portals
failed=0

clean() { printf '%s' "$*" | tr '\n"' "  "; }

# run_case <name> <expected-exit> <module> <fragment>... [-- host args...]
run_case() {
	name="$1" want_rc="$2" module="$3"
	shift 3
	wants=""
	while [ $# -gt 0 ] && [ "$1" != -- ]; do
		wants="$wants$1
"
		shift
	done
	[ "${1:-}" = -- ] && shift
	expected="exit $want_rc and log lines containing: $(printf '%s' "$wants" | paste -sd ';' -)"
	if [ ! -f "$DIR/$module.wasm" ]; then
		printf 'PORTALS step=%s result=fail expected="%s" got="%s"\n' "$name" \
			"$(clean "$expected")" "$(clean "$DIR/$module.wasm missing from the initramfs")"
		failed=1
		return
	fi
	out="$("$HOST" --device "$BDF" --module "$DIR/$module.wasm" "$@" 2>&1)"
	rc=$?
	printf '%s\n' "$out" | sed 's/^/    /'
	result=pass
	[ "$rc" -eq "$want_rc" ] || result=fail
	missing=""
	while IFS= read -r want; do
		[ -n "$want" ] || continue
		printf '%s' "$out" | grep -qF -- "$want" || { result=fail; missing="$missing [$want]"; }
	done <<-EOW
	$wants
	EOW
	[ "$result" = pass ] || failed=1
	got="exit $rc; last line: $(printf '%s\n' "$out" | tail -n 1)"
	[ -z "$missing" ] || got="$got; missing:$missing"
	printf 'PORTALS step=%s result=%s expected="%s" got="%s"\n' "$name" "$result" \
		"$(clean "$expected")" "$(clean "$got")"
}

# The RTL8139 deny list is not reviewed yet (rule 10), so cases that open
# the device must allow that explicitly. This is valid only for emulated
# devices.
EMU=--allow-unreviewed-deny-list
DENIED='"status":"fail","reason":"deny-list-violation"'

# Gate: without the flag, the unreviewed list keeps the device closed.
run_case rtl-gate 2 rtl-read-id '"step":"device-deny-list"' 'list for Realtek RTL8139' 'not reviewed yet'

# The window is BAR1 (memory), 256 bytes, and holds the registers: IDR0-5
# read one byte at a time give vm/run.sh's MAC address.
run_case rtl-bar1 0 rtl-read-id \
	'"model":"Realtek RTL8139"' '"bar":1,"bar_size":"0x100"' '"list_kind":"deny"' \
	'"message":"IDR","field.mac":"52:54:00:12:34:56","field.window":"256"' '"status":"pass"' -- $EMU
run_case rtl-reset-at-open 0 rtl-read-id '"reset":"VFIO_DEVICE_RESET ok"' -- $EMU
run_case rtl-widths 0 rtl-widths '"message":"IMR","field.value":"0x0005"' '"status":"pass"' -- $EMU

# The deny list (rule 10). Each module handles the error; the host must
# still fail the run.
run_case rtl-9346cr 1 rtl-9346cr \
	'"event":"mmio-denied","attempted":"write8","offset":"0x50","value":"0x80","register":"9346CR"' "$DENIED" -- $EMU
run_case rtl-flash 1 rtl-flash \
	'"event":"mmio-denied","attempted":"write32","offset":"0xd4","value":"0x20000","register":"FLASH"' "$DENIED" -- $EMU
run_case rtl-config-dword 1 rtl-config-dword \
	'"event":"mmio-denied","attempted":"write32","offset":"0x58","value":"0x0","register":"CONFIG3"' "$DENIED" -- $EMU

# Teardown: bus mastering off before the driver's buffers are unmapped.
run_case rtl-quiesce 0 rtl-read-id '"event":"device-quiesce","bdf":"'"$BDF"'"' -- $EMU

# After every case above: the IOMMU logged no DMA faults.
expected="no DMAR fault lines in dmesg after all cases"
faults="$(dmesg | grep -E 'DMAR: \[DMA (Read|Write)|DMAR: DRHD: handling fault' || true)"
if [ -z "$faults" ]; then
	result=pass got="none"
else
	result=fail got="$(printf '%s\n' "$faults" | head -n 5)"
	failed=1
	printf '%s\n' "$faults" | sed 's/^/    /'
fi
printf 'PORTALS step=rtl-no-dma-faults result=%s expected="%s" got="%s"\n' "$result" "$expected" "$(clean "$got")"

exit "$failed"
