#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Boot the test VM: q35 + KVM, emulated Intel IOMMU, an emulated network card
# (e1000e by default, or RTL8139), and optionally an emulated NVMe controller.
#
#   vm/run.sh                 interactive; serial console on this terminal,
#                             shell after the checks (Ctrl-A x quits QEMU)
#   vm/run.sh --check         headless; exits 0 only if every guest check passes
#
# Options:
#   --timeout SECONDS         check mode: give up after this long (default 90)
#   --log FILE                check mode: serial log (default vm/build/serial.log)
#   --run GUEST_PATH          after the VM checks, run this initramfs program
#                             with the device under test's PCI address
#                             (see build-initramfs.sh --add)
#   --net-udp PEER:LOCAL      connect the network card to a UDP socket instead of
#                             user-mode networking: QEMU binds 127.0.0.1:LOCAL
#                             and sends each frame as one datagram to
#                             127.0.0.1:PEER (see tests/nic)
#   --nvme IMAGE              add an emulated NVMe controller (1b36:0010)
#                             backed by the raw disk image IMAGE, behind its
#                             own PCIe root port, and make it the device
#                             under test instead of the e1000e. IMAGE is
#                             written to: use a scratch image (see tests/block)
#   --seed N                  pass grind.seed=N to the guest (tests use it to
#                             seed random choices, so a failure can be replayed)
#   --qmp SOCKET              open a QEMU monitor (QMP) on this Unix socket, so
#                             a test can change the VM while it runs (for
#                             example, set_link to drop the network link)
#   --nic MODEL               the network card: e1000e (default, on the root
#                             bus) or rtl8139 (behind its own PCIe root port,
#                             and the device under test)
#
# The network card's MAC address is always 52:54:00:12:34:56. --nvme and
# --nic rtl8139 each choose the device under test, so they cannot be combined.
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
NETDEV="user,id=net0"
NVME=""
NIC=e1000e
QMP=""
SEED=""

usage() {
	sed -n '4,38p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
	exit 2
}

while [ $# -gt 0 ]; do
	case "$1" in
	--check) MODE=check ;;
	--timeout) TIMEOUT="${2:?--timeout needs a value}"; shift ;;
	--log) LOG="${2:?--log needs a value}"; shift ;;
	--run) RUN="${2:?--run needs a value}"; shift ;;
	--net-udp)
		ports="${2:?--net-udp needs PEER:LOCAL}"
		[[ "$ports" =~ ^([0-9]{1,5}):([0-9]{1,5})$ ]] ||
			{ echo "run.sh: --net-udp takes PEER:LOCAL port numbers, got: $ports" >&2; exit 2; }
		NETDEV="socket,id=net0,udp=127.0.0.1:${BASH_REMATCH[1]},localaddr=127.0.0.1:${BASH_REMATCH[2]}"
		shift
		;;
	--nvme) NVME="${2:?--nvme needs a disk image}"; shift ;;
	--nic) NIC="${2:?--nic needs a model}"; shift ;;
	--qmp) QMP="${2:?--qmp needs a socket path}"; shift ;;
	--seed)
		SEED="${2:?--seed needs a number}"
		[[ "$SEED" =~ ^[0-9]{1,19}$ ]] || { echo "run.sh: --seed takes a decimal number, got: $SEED" >&2; exit 2; }
		shift
		;;
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

case "$NIC" in
e1000e | rtl8139) ;;
*) fail args "--nic e1000e or --nic rtl8139" "$NIC" ;;
esac
[ -z "$NVME" ] || [ "$NIC" = e1000e ] ||
	fail args "--nvme or --nic $NIC, not both (each chooses the device under test)" "both"

# The NVMe image must be a writable regular file. QEMU's -drive option
# splits on commas, so a comma in the path is refused instead of escaped.
if [ -n "$NVME" ]; then
	case "$NVME" in *,*) fail args "--nvme path without commas" "$NVME" ;; esac
	[ -f "$NVME" ] || fail nvme-image "$NVME is a regular file" "missing or not a regular file"
	[ -r "$NVME" ] && [ -w "$NVME" ] || fail nvme-image "$NVME readable and writable by $(id -un)" "$(ls -l "$NVME")"
	NVME="$(realpath "$NVME")"
fi

APPEND="console=ttyS0 intel_iommu=on loglevel=4 panic=-1 grind.mode=$MODE"
[ -z "$RUN" ] || APPEND="$APPEND grind.run=$RUN"
[ -z "$SEED" ] || APPEND="$APPEND grind.seed=$SEED"
[ -z "$NVME" ] || APPEND="$APPEND grind.device=1b36:0010"
[ "$NIC" != rtl8139 ] || APPEND="$APPEND grind.device=10ec:8139"

QEMU_ARGS=(
	-machine q35,accel=kvm,kernel-irqchip=split
	-cpu host -smp 2 -m 1024
	-nodefaults -no-user-config -display none -no-reboot
	# The IOMMU must be created before the devices it translates for.
	-device intel-iommu,intremap=on
	-netdev "$NETDEV"
	-kernel "$BUILD/vmlinuz"
	-initrd "$BUILD/initramfs.gz"
	-append "$APPEND"
)
if [ -n "$QMP" ]; then
	case "$QMP" in *,*) fail args "--qmp path without commas" "$QMP" ;; esac
	QEMU_ARGS+=(-qmp "unix:$QMP,server=on,wait=off")
fi
# The network card. Like the NVMe controller below, the RTL8139 has no reset
# method on the root bus, so it gets its own PCIe root port.
case "$NIC" in
e1000e) QEMU_ARGS+=(-device e1000e,netdev=net0,mac=52:54:00:12:34:56) ;;
rtl8139)
	QEMU_ARGS+=(
		-device pcie-root-port,id=rp2,chassis=2
		-device rtl8139,netdev=net0,mac=52:54:00:12:34:56,bus=rp2
	)
	;;
esac
# On the root bus QEMU's NVMe controller has no reset method, and the shim
# refuses a device it cannot reset. Behind its own PCIe root port it gets a
# secondary bus reset and its own IOMMU group.
if [ -n "$NVME" ]; then
	QEMU_ARGS+=(
		-device pcie-root-port,id=rp1,chassis=1
		-drive "file=$NVME,if=none,id=nvm,format=raw"
		-device nvme,serial=grind0001,drive=nvm,bus=rp1
	)
fi

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
