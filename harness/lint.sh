#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Lint the trusted Rust code before a trusted commit (issues #7, #8):
#
#   harness/lint.sh
#
#   1. cargo fmt --check over the workspace (shim, host, tests/nic/peer,
#      tests/block/wbdisk) and every test-module crate (tests/*/modules).
#   2. cargo clippy over the workspace, all targets, warnings as errors.
#
# Drivers are not linted here: harness/grind.sh checks a driver's
# formatting as one of its rule checks.
#
# Prints LINT step=... lines and ends with LINT result=pass|fail.
# Exit 0 only if every step passes.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
failed=0

step() { # name pass|fail detail
	printf 'LINT step=%s result=%s %s\n' "$1" "$2" "$3"
	[ "$2" = pass ] || failed=1
}

fmt() { # name manifest
	local out
	if out="$(cargo fmt --manifest-path "$2" --all --check 2>&1)"; then
		step "$1" pass "cargo fmt --check: clean"
	else
		step "$1" fail "cargo fmt --check: $(grep -c '^Diff in' <<<"$out") differences; first: $(grep -m1 '^Diff in' <<<"$out" | sed "s|$ROOT/||") (run cargo fmt)"
	fi
}

fmt fmt-workspace "$ROOT/Cargo.toml"
for m in "$ROOT"/tests/*/modules/Cargo.toml; do
	rel="${m#"$ROOT"/}"
	fmt "fmt-${rel%/Cargo.toml}" "$m"
done

if out="$(cargo clippy --manifest-path "$ROOT/Cargo.toml" --workspace --all-targets --locked -- -D warnings 2>&1)"; then
	step clippy pass "cargo clippy --workspace --all-targets -D warnings: clean"
else
	step clippy fail "$(grep -cE '^(warning|error)' <<<"$out") findings; first: $(grep -m1 -E '^(warning|error)' <<<"$out")"
fi

if [ "$failed" -eq 0 ]; then echo "LINT result=pass"; else echo "LINT result=fail"; fi
exit "$failed"
