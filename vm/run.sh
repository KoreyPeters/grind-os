#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Boot the test VM: q35 + KVM, emulated Intel IOMMU, emulated e1000e.
#
#   vm/run.sh                 interactive; serial console on this terminal,
#                             shell after the checks (Ctrl-A x quits QEMU)
#   vm/run.sh --check         headless; exits 0 only if every guest check passes
#
# Options:
#   --timeout SECONDS         check mode: give up after this long (default 90)
#   --log FILE                check mode: serial log (default vm/build/serial.log)
#   --run GUEST_PATH          after the VM checks, run this initramfs program
#                             with the e1000e's PCI address (see build-initramfs.sh --add)
#
# Host-side failures print one line of the form
#   VMRUN step=<step> expected="..." got="..."
# Guest-side results are the VMCHECK lines written by vm/init, plus any
# "<PREFIX> step=... result=..." lines printed by the --run program.

set -euo pipefail

VM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD="$VM_DIR/build"

MODE=shell
TIMEOUT=90
LOG="$BUILD/serial.log"
RUN=""

usage() {
	sed -n '4,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
	exit 2
}

while [ $# -gt 0 ]; do
	case "$1" in
	--check) MODE=check ;;
	--timeout) TIMEOUT="${2:?--timeout needs a value}"; shift ;;
	--log) LOG="${2:?--log needs a value}"; shift ;;
	--run) RUN="${2:?--run needs a value}"; shift ;;
	-h | --help) usage ;;
	*) echo "run.sh: unknown argument: $1" >&2; usage ;;
	esac
	shift
done

fail() { # step expected got
	printf 'VMRUN step=%s expected="%s" got="%s"\n' "$1" "$2" "$3" >&2
	exit 1
}

# Preflight.
QEMU=qemu-system-x86_64
command -v "$QEMU" >/dev/null || fail qemu "$QEMU on PATH" "not found"
[ -e /dev/kvm ] || fail kvm "/dev/kvm exists" "missing (is nested virtualization enabled in WSL?)"
[ -r /dev/kvm ] && [ -w /dev/kvm ] ||
	fail kvm "/dev/kvm readable and writable by $(id -un) (see vm/README.md)" "$(ls -l /dev/kvm)"
for f in vmlinuz initramfs.gz; do
	[ -f "$BUILD/$f" ] || fail inputs "$BUILD/$f (run vm/fetch.sh && vm/build-initramfs.sh)" "missing"
done
case "$RUN" in
"") ;;
*[[:space:]]*) fail args "--run path without whitespace" "$RUN" ;;
/*) ;;
*) fail args "--run takes an absolute guest path" "$RUN" ;;
esac

APPEND="console=ttyS0 intel_iommu=on loglevel=4 panic=-1 grind.mode=$MODE"
[ -z "$RUN" ] || APPEND="$APPEND grind.run=$RUN"

QEMU_ARGS=(
	-machine q35,accel=kvm,kernel-irqchip=split
	-cpu host -smp 2 -m 512
	-nodefaults -no-user-config -display none -no-reboot
	# The IOMMU must be created before the devices it translates for.
	-device intel-iommu,intremap=on
	-netdev user,id=net0
	-device e1000e,netdev=net0
	-kernel "$BUILD/vmlinuz"
	-initrd "$BUILD/initramfs.gz"
	-append "$APPEND"
)

if [ "$MODE" = shell ]; then
	exec "$QEMU" "${QEMU_ARGS[@]}" -serial mon:stdio
fi

# Check mode.
mkdir -p "$(dirname "$LOG")"
set +e
timeout "$TIMEOUT" "$QEMU" "${QEMU_ARGS[@]}" -serial stdio </dev/null | tee "$LOG"
status="${PIPESTATUS[0]}"
set -e

lines="$(tr -d '\r' <"$LOG" | grep -aE '^[A-Z]+ (step|result)=' || true)"
result="$(grep '^VMCHECK result=' <<<"$lines" | tail -n 1 || true)"

echo
if [ "$status" -eq 124 ]; then
	fail timeout "guest powers off within ${TIMEOUT}s" \
		"still running; last line: $(tail -n 1 <<<"$lines") (log: $LOG)"
fi
[ "$status" -eq 0 ] || fail qemu "qemu exits 0" "exit $status (log: $LOG)"
[ -n "$result" ] || fail result "a 'VMCHECK result=' line in the serial log" "none (log: $LOG)"

if [ "$result" = "VMCHECK result=pass" ]; then
	echo "vm: check passed ($(grep -c 'result=pass expected' <<<"$lines") steps; log: $LOG)"
	exit 0
fi
echo "vm: check failed (log: $LOG)" >&2
grep 'result=fail' <<<"$lines" >&2
exit 1
