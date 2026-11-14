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
run_case unreviewed-refused 2 '"step":"device-deny-list"' read-status
run_case denied-write 1 '"event":"mmio-denied","attempted":"write32","offset":"0x102c"' denied-write $EMU

exit "$failed"
