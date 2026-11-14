#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Runs inside the test VM booted with vm/run.sh --nvme (vm/init:
# grind.run=/tests/portals/guest-nvme.sh), against QEMU's NVMe controller.
# Checks the shim's side of rule 10 for a device that fetches commands:
# controller bring-up, the nvme portal, command refusals and the register
# allow list. Each case runs grind-host with one test module and checks its
# exit status and every expected log fragment.
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

# The QEMU NVMe register and command lists are not reviewed yet (rule 10),
# so cases that open the device must allow that explicitly. This is valid
# only for emulated devices.
EMU=--allow-unreviewed-deny-list
REFUSED='"status":"fail","reason":"command-refused"'
DENIED='"status":"fail","reason":"deny-list-violation"'

# Gate: without the flag, the unreviewed lists keep the device closed.
run_case nvme-gate 2 nvme-smoke '"step":"device-deny-list"' 'and command list for QEMU NVM Express controller' 'not reviewed yet'

# Bring-up, then every allowed path once: Identify, Set Features, queue
# creation, Write, Read, Flush, and collecting completions.
run_case nvme-smoke 0 nvme-smoke \
	'"event":"nvme-ready"' '"dstrd":0' '"commands_reviewed":false' \
	'"message":"nvme-smoke","field.serial":"grind0001","field.round_trip_bytes":"4096"' \
	'"status":"pass"' -- $EMU

# Flow control: the shim learns how far the device has fetched only from
# completions it reads itself, so an SQ fills until they are collected.
run_case nvme-queue-full 0 nvme-queue-full \
	'"field.queue_full_after":"15","field.then":"ok"' '"got":"queue-full (tail 15, device head 0); collect completions first"' \
	'"status":"pass"' -- $EMU

# Commands the shim must refuse (rule 10). Each module handles the error;
# the host must still fail the run.
run_case nvme-format 1 nvme-format \
	'"event":"command-refused","queue":"admin","sqid":0,"opcode":"0x80","command":"unlisted","check":"opcode"' \
	"$REFUSED" -- $EMU
run_case nvme-save 1 nvme-save \
	'"opcode":"0x9","command":"Set Features (Number of Queues)","check":"feature"' \
	"$REFUSED" -- $EMU
run_case nvme-vendor-admin 1 nvme-vendor-admin \
	'"opcode":"0xc0","command":"unlisted","check":"opcode"' "$REFUSED" -- $EMU
run_case nvme-unlisted-io 1 nvme-unlisted-io \
	'"queue":"io","sqid":1,"opcode":"0x8","command":"unlisted","check":"opcode"' "$REFUSED" -- $EMU
run_case nvme-prp-shim 1 nvme-prp-shim \
	'"command":"Read","check":"address"' 'PRP1 range 0x100000000+0x200' "$REFUSED" -- $EMU
run_case nvme-prp-list 1 nvme-prp-list \
	'"command":"Read","check":"prp-list"' "$REFUSED" -- $EMU
run_case nvme-cq-in-driver 1 nvme-cq-in-driver \
	'"command":"Create I/O Completion Queue","check":"prp"' "$REFUSED" -- $EMU

# Registers the shim owns: the allow list refuses them.
run_case nvme-sq-doorbell 1 nvme-sq-doorbell \
	'"event":"mmio-denied","attempted":"write32","offset":"0x1008","value":"0x1","register":"unlisted"' \
	"$DENIED" -- $EMU
run_case nvme-cq-doorbell 1 nvme-cq-doorbell \
	'"event":"mmio-denied","attempted":"write32","offset":"0x100c","value":"0x1","register":"unlisted"' \
	"$DENIED" -- $EMU
run_case nvme-cc-write 1 nvme-cc-write \
	'"event":"mmio-denied","attempted":"write32","offset":"0x14","value":"0x0","register":"unlisted"' \
	"$DENIED" -- $EMU

# Teardown: bus mastering off before the driver's buffers are unmapped.
run_case nvme-quiesce 0 nvme-smoke '"event":"device-quiesce","bdf":"'"$BDF"'"' -- $EMU

# After every case above: the IOMMU logged no DMA faults. The refused
# commands never reached the device, so nothing faulted.
expected="no DMAR fault lines in dmesg after all cases"
faults="$(dmesg | grep -E 'DMAR: \[DMA (Read|Write)|DMAR: DRHD: handling fault' || true)"
if [ -z "$faults" ]; then
	result=pass got="none"
else
	result=fail got="$(printf '%s\n' "$faults" | head -n 5)"
	failed=1
	printf '%s\n' "$faults" | sed 's/^/    /'
fi
printf 'PORTALS step=nvme-no-dma-faults result=%s expected="%s" got="%s"\n' "$result" "$expected" "$(clean "$got")"

exit "$failed"
