#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Milestone 2 end to end: build vfio-hello as a static binary, put it in the
# test VM's initramfs, boot the VM and run it against the e1000e.
# Exits 0 only if the VM checks and every vfio-hello step pass.
#
# Requires vm/fetch.sh to have run once.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=x86_64-unknown-linux-gnu
BIN="$ROOT/hello/target/$TARGET/release/vfio-hello"

fail() { # step expected got
	printf 'HELLO step=%s expected="%s" got="%s"\n' "$1" "$2" "$3" >&2
	exit 1
}

# The guest has no shared libraries, so link glibc statically. An explicit
# --target keeps the static flag away from anything built for the host.
RUSTFLAGS="-C target-feature=+crt-static" \
	cargo build --release --locked --manifest-path "$ROOT/hello/Cargo.toml" --target "$TARGET" ||
	fail build "cargo build succeeds" "exit $?"
# Static means no ELF interpreter and no shared-library dependencies. (Older
# `file` versions call a static PIE "dynamically linked", so ask readelf.)
dyn="$(readelf -lW "$BIN" | grep -i 'program interpreter' || true)$(readelf -dW "$BIN" | grep NEEDED || true)"
[ -z "$dyn" ] || fail static "$BIN has no interpreter and no NEEDED libraries" "$dyn"

"$ROOT/vm/build-initramfs.sh" --add "$BIN=/bin/vfio-hello"
"$ROOT/vm/run.sh" --check --run /bin/vfio-hello "$@"
