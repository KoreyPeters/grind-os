#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Milestone 3 conformance: the shim and host.
#   1. Shim unit tests on this machine (bounds, alignment, deny list, logging).
#   2. Build grind-host (static) and the Wasm test modules.
#   3. Boot the test VM and run tests/portals/guest.sh against the e1000e.
#   4. Boot it again with an emulated NVMe controller backed by a fresh
#      scratch image, and run tests/portals/guest-nvme.sh against that.
#   5. Boot it a third time with an RTL8139 as the network card, and run
#      tests/portals/guest-rtl8139.sh against that.
# Exits 0 only if everything passes. Requires vm/fetch.sh to have run once.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET=x86_64-unknown-linux-gnu
HOST_BIN="$ROOT/target/$TARGET/release/grind-host"
MODULES="$ROOT/tests/portals/modules"
WASM="$MODULES/target/wasm32-unknown-unknown/release"

fail() { # step expected got
	printf 'PORTALS step=%s result=fail expected="%s" got="%s"\n' "$1" "$2" "$3" >&2
	exit 1
}

cargo test --locked --manifest-path "$ROOT/Cargo.toml" -p grind-shim ||
	fail shim-tests "cargo test -p grind-shim passes" "exit $?"

# Static glibc for the guest; an explicit --target keeps the flag off
# build scripts and proc macros.
RUSTFLAGS="-C target-feature=+crt-static" \
	cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" -p grind-host --target "$TARGET" ||
	fail build-host "cargo build -p grind-host succeeds" "exit $?"
dyn="$(readelf -lW "$HOST_BIN" | grep -i 'program interpreter' || true)$(readelf -dW "$HOST_BIN" | grep NEEDED || true)"
[ -z "$dyn" ] || fail static "$HOST_BIN has no interpreter and no NEEDED libraries" "$dyn"

cargo build --release --locked --manifest-path "$MODULES/Cargo.toml" --target wasm32-unknown-unknown ||
	fail build-modules "test modules build for wasm32-unknown-unknown" "exit $?"

adds=(--add "$HOST_BIN=/bin/grind-host"
	--add "$ROOT/tests/portals/guest.sh=/tests/portals/guest.sh"
	--add "$ROOT/tests/portals/guest-nvme.sh=/tests/portals/guest-nvme.sh"
	--add "$ROOT/tests/portals/guest-rtl8139.sh=/tests/portals/guest-rtl8139.sh")
for wasm in "$WASM"/portal_test_*.wasm; do
	name="$(basename "$wasm" .wasm)"
	name="${name#portal_test_}"
	adds+=(--add "$wasm=/tests/portals/${name//_/-}.wasm")
done
"$ROOT/vm/build-initramfs.sh" "${adds[@]}"
"$ROOT/vm/run.sh" --check --run /tests/portals/guest.sh --timeout 180 "$@" ||
	fail vm-e1000e "tests/portals/guest.sh passes against the e1000e" "exit $?"

# A scratch image the NVMe cases may write to; recreated every run.
IMAGE="$ROOT/target/portals-nvme.img"
rm -f "$IMAGE"
truncate -s 16M "$IMAGE"
"$ROOT/vm/run.sh" --check --nvme "$IMAGE" --run /tests/portals/guest-nvme.sh --timeout 180 "$@" ||
	fail vm-nvme "tests/portals/guest-nvme.sh passes against the NVMe controller" "exit $?"

"$ROOT/vm/run.sh" --check --nic rtl8139 --run /tests/portals/guest-rtl8139.sh --timeout 180 "$@" ||
	fail vm-rtl8139 "tests/portals/guest-rtl8139.sh passes against the RTL8139" "exit $?"
