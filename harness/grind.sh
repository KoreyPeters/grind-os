#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# One grind iteration for a driver:
#
#   harness/grind.sh <driver-name> [vm/run.sh options]
#
#   1. Rule checks on drivers/<name>/ (CLAUDE.md rules 2, 6, 7, 8).
#   2. Build the driver for wasm32-unknown-unknown.
#   3. Run its device class's conformance test (tests/<class>/check.sh).
#   4. Keep everything in harness/runs/<name>/<UTC time>/.
#
# Prints GRIND step=... lines for its own steps and ends with
#   GRIND driver=<name> result=pass|fail run=<dir>
# Exit 0 only if every check and the conformance test pass.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

[ $# -ge 1 ] || { echo "usage: harness/grind.sh <driver-name> [vm/run.sh options]" >&2; exit 2; }
NAME="$1"
shift

# Which conformance test each driver must pass, and the VM options it needs.
# Kept here (trusted), not in the driver's directory, so a driver cannot
# choose its own test or device.
VMARGS=()
case "$NAME" in
e1000e) CLASS=nic ;;
nvme) CLASS=block ;;
rtl8139) CLASS=nic VMARGS=(--nic rtl8139) ;;
*) echo "GRIND step=driver result=fail expected=\"a driver listed in harness/grind.sh\" got=\"$NAME\""; exit 2 ;;
esac

DIR="$ROOT/drivers/$NAME"
RUN="$ROOT/harness/runs/$NAME/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$RUN"
LOG="$RUN/output.log"
failed=0

say() { printf '%s\n' "$*" | tee -a "$LOG"; }
check() { # step pass|fail expected got
	say "GRIND step=$1 result=$2 expected=\"$3\" got=\"$(printf '%s' "$4" | tr '\n"' "  ")\""
	[ "$2" = pass ] || failed=1
}
finish() {
	git -C "$ROOT" rev-parse HEAD >"$RUN/commit" 2>/dev/null
	git -C "$ROOT" status --short >"$RUN/status" 2>/dev/null
	local result=pass
	[ "$failed" -eq 0 ] || result=fail
	say "GRIND driver=$NAME result=$result run=${RUN#"$ROOT"/}"
	exit "$failed"
}

# 1. Rule checks.
[ -d "$DIR" ] || { check exists fail "$DIR exists" "missing"; finish; }

for f in SPEC.md PROVENANCE.md; do # rules 6 and 7
	if [ -s "$DIR/$f" ]; then check "$f" pass "drivers/$NAME/$f exists" "present"
	else check "$f" fail "drivers/$NAME/$f exists" "missing or empty"; fi
done

# Rule 8: SPDX header in the first three lines of every source file.
missing="$(cd "$DIR" && find . -path ./target -prune -o -type f \
	\( -name '*.rs' -o -name '*.toml' -o -name '*.md' -o -name '*.sh' -o -name '*.wit' \) -print |
	while read -r f; do head -n 3 "$f" | grep -q 'SPDX-License-Identifier: AGPL-3.0-or-later' || echo "$f"; done)"
if [ -z "$missing" ]; then check spdx pass "every source file has an AGPL-3.0-or-later SPDX header" "all do"
else check spdx fail "every source file has an AGPL-3.0-or-later SPDX header" "missing in: $missing"; fi

# Rule 1: while grinding, only drivers/<name>/ may change. Any uncommitted
# or untracked change elsewhere in the repository (trusted code, tests,
# this harness) fails the grind. Ignored files (target/, harness/runs/)
# don't count.
if outside="$(git -C "$ROOT" status --porcelain --untracked-files=all -- . ":(exclude)drivers/$NAME" 2>&1)"; then
	if [ -z "$outside" ]; then
		check trusted-unchanged pass "no changes outside drivers/$NAME (rule 1)" "none"
	else
		check trusted-unchanged fail "no changes outside drivers/$NAME (rule 1)" \
			"$(printf '%s\n' "$outside" | sed 's/^ *//' | paste -sd ';' - | sed 's/;/; /g')"
	fi
else
	check trusted-unchanged fail "git status succeeds in $ROOT (rule 1)" "$outside"
fi

# Rule 2: no unsafe in driver source.
hits="$(grep -rnw --include='*.rs' unsafe "$DIR" --exclude-dir=target || true)"
if [ -z "$hits" ]; then check no-unsafe pass "no 'unsafe' in drivers/$NAME" "none"
else check no-unsafe fail "no 'unsafe' in drivers/$NAME" "$hits"; fi

# Nothing may run on this machine at build time, and the only dependency
# is wit-bindgen (whose generated glue is the one place unsafe is allowed).
if find "$DIR" -path "$DIR/target" -prune -o -name build.rs -print | grep -q . ||
	grep -qE '^\s*build\s*=' "$DIR/Cargo.toml" 2>/dev/null; then
	check no-build-script fail "no build script" "build.rs or build = present"
else
	check no-build-script pass "no build script" "none"
fi
if [ ! -f "$DIR/Cargo.toml" ]; then
	check dependencies fail "only wit-bindgen" "drivers/$NAME/Cargo.toml missing"
elif ! meta="$(cargo metadata --format-version 1 --manifest-path "$DIR/Cargo.toml" --no-deps 2>"$RUN/metadata.err")"; then
	check dependencies fail "only wit-bindgen" "cargo metadata failed: $(head -n 3 "$RUN/metadata.err")"
else
	deps="$(python3 -I -c 'import json,sys; d=json.load(sys.stdin); print(" ".join(sorted({x["name"] for p in d["packages"] for x in p["dependencies"]})) or "none")' <<<"$meta")"
	if [ "$deps" = wit-bindgen ]; then check dependencies pass "only wit-bindgen" "$deps"
	else check dependencies fail "only wit-bindgen" "$deps"; fi
fi

# 2. Build.
cargo build --release --locked --manifest-path "$DIR/Cargo.toml" --target wasm32-unknown-unknown \
	>"$RUN/build.log" 2>&1
rc=$?
wasm="$(find "$DIR/target/wasm32-unknown-unknown/release" -maxdepth 1 -name '*.wasm' 2>/dev/null | head -n 1)"
if [ "$rc" -eq 0 ] && [ -n "$wasm" ]; then
	cp "$wasm" "$RUN/driver.wasm"
	check build pass "cargo build for wasm32-unknown-unknown produces a .wasm" "$(basename "$wasm"), $(stat -c %s "$wasm") bytes"
else
	check build fail "cargo build for wasm32-unknown-unknown produces a .wasm" \
		"exit $rc; $(grep -E '^error' "$RUN/build.log" | head -n 5)"
	tail -n 40 "$RUN/build.log" | sed 's/^/    /' | tee -a "$LOG"
fi

if [ "$failed" -ne 0 ]; then
	say "GRIND step=conformance result=fail expected=\"rule checks and build pass first\" got=\"skipped\""
	finish
fi

# 3. Conformance.
"$ROOT/tests/$CLASS/check.sh" "$RUN/driver.wasm" "${VMARGS[@]}" "$@" 2>&1 | tee -a "$LOG"
rc="${PIPESTATUS[0]}"
cp "$ROOT/vm/build/serial.log" "$RUN/serial.log" 2>/dev/null
[ "$CLASS" = nic ] && cp "$ROOT/target/nic-test/peer.log" "$RUN/peer.log" 2>/dev/null
if [ "$rc" -eq 0 ]; then check conformance pass "tests/$CLASS/check.sh passes" "exit 0"
else check conformance fail "tests/$CLASS/check.sh passes" "exit $rc"; fi
finish
