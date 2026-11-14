#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# NIC conformance (milestone 4): bring the link up and answer a ping.
#
#   tests/nic/check.sh <driver.wasm> [vm/run.sh options]
#
#   1. Build grind-host (static) and nic-peer.
#   2. Start nic-peer on this machine; it reserves the UDP ports for the wire.
#   3. Boot the test VM with the e1000e on that wire and run tests/nic/guest.sh,
#      which serves ARP and ping for 10.0.2.15 through the driver.
#   4. Pass only if the VM run passes and the peer got valid ARP and ping
#      replies.
#
# Prints NIC step=... lines and ends with "NIC result=pass|fail".
# Requires vm/fetch.sh to have run once.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET=x86_64-unknown-linux-gnu
HOST_BIN="$ROOT/target/$TARGET/release/grind-host"
PEER_BIN="$ROOT/target/release/nic-peer"
WORK="$ROOT/target/nic-test"

fail() { # step expected got
	printf 'NIC step=%s result=fail expected="%s" got="%s"\n' "$1" "$2" "$3"
	echo "NIC result=fail"
	exit 1
}

[ $# -ge 1 ] || fail args "tests/nic/check.sh <driver.wasm> [vm/run.sh options]" "no driver given"
DRIVER="$(realpath "$1")"
shift
[ -f "$DRIVER" ] || fail args "a driver module at $DRIVER" "missing"

RUSTFLAGS="-C target-feature=+crt-static" \
	cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" -p grind-host --target "$TARGET" ||
	fail build-host "cargo build -p grind-host succeeds" "exit $?"
cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" -p nic-peer ||
	fail build-peer "cargo build -p nic-peer succeeds" "exit $?"

"$ROOT/vm/build-initramfs.sh" \
	--add "$HOST_BIN=/bin/grind-host" \
	--add "$ROOT/tests/nic/guest.sh=/tests/nic/guest.sh" \
	--add "$DRIVER=/drivers/driver.wasm"

mkdir -p "$WORK"
PORTS="$WORK/ports"
PEER_LOG="$WORK/peer.log"
STOP="$WORK/vm-exited"
# QEMU's monitor, through which the peer drops and raises the link. In a
# short temporary directory: a Unix socket path must be under 108 bytes,
# and the repository may be deep (a worktree, for example).
QMP_DIR="$(mktemp -d /tmp/grind-qmp.XXXXXX)"
QMP="$QMP_DIR/qmp.sock"
# The seed for the host's injected faults: random each run unless
# NIC_FAULT_SEED is set, and printed so a failure can be replayed.
SEED="${NIC_FAULT_SEED:-$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')}"
echo "NIC fault-seed=$SEED (replay with NIC_FAULT_SEED=$SEED)"
rm -f "$PORTS" "$STOP"
"$PEER_BIN" --ports-file "$PORTS" --stop-file "$STOP" --deadline-s 150 --qmp "$QMP" >"$PEER_LOG" 2>&1 &
peer=$!
trap 'kill "$peer" 2>/dev/null || true; rm -rf "$QMP_DIR"' EXIT

for _ in $(seq 50); do
	[ -s "$PORTS" ] && break
	sleep 0.1
done
[ -s "$PORTS" ] || fail peer-ports "nic-peer writes its ports within 5 s" "$(cat "$PEER_LOG")"
read -r peer_port qemu_port <"$PORTS"

set +e
"$ROOT/vm/run.sh" --check --run /tests/nic/guest.sh --net-udp "$peer_port:$qemu_port" --qmp "$QMP" --seed "$SEED" --timeout 180 "$@"
vm_status=$?

# The peer normally finishes during the serve window. If it is still
# waiting, nothing will answer it now: tell it to report and stop.
touch "$STOP"
for _ in $(seq 50); do
	kill -0 "$peer" 2>/dev/null || break
	sleep 0.1
done
if kill -0 "$peer" 2>/dev/null; then
	kill "$peer" 2>/dev/null
	wait "$peer" 2>/dev/null
	peer_status=timeout
else
	wait "$peer"
	peer_status=$?
fi
set -e

echo
echo "nic-peer (log: $PEER_LOG):"
cat "$PEER_LOG"
if [ "$peer_status" = timeout ]; then
	printf 'NIC step=peer result=fail expected="%s" got="%s"\n' \
		"nic-peer reports within 5 s of the VM powering off" "still running; killed"
fi

if [ "$vm_status" -eq 0 ] && [ "$peer_status" = 0 ]; then
	echo "NIC result=pass"
	exit 0
fi
printf 'NIC step=summary result=fail expected="%s" got="%s"\n' \
	"VM run exits 0 and nic-peer passes" "vm exit $vm_status, peer $peer_status"
echo "NIC result=fail"
exit 1
