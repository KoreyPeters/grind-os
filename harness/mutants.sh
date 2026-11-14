#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# How strong is a driver's conformance test? Break the driver on purpose and
# count how many breaks the test catches.
#
#   harness/mutants.sh <driver-name> [--max N] [--seed S] [vm/run.sh options]
#
#   1. The unmutated driver must pass its conformance test (the class comes
#      from harness/classes.sh); otherwise a score means nothing.
#   2. harness/mutate.py lists every mutation of drivers/<name>/src/lib.rs
#      (one typical mistake at one place). A seeded sample of --max of them
#      (default 20, seed 1) is tested.
#   3. Each mutant is built in target/mutants/<name>/ and run through the
#      conformance test: caught if the test fails, survived if it passes,
#      unbuildable if it does not compile (not counted).
#
# Prints a MUTANT line per mutant and ends with
#   MUTANTS driver=<name> caught=C survived=S unbuildable=U score=C/(C+S)
# then every survivor: each is a gap in the test (or a mutation that
# changes nothing). Logs: harness/runs/<name>/mutants-<UTC time>/.
# Exit 0 if every built mutant was caught.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[ $# -ge 1 ] || { echo "usage: harness/mutants.sh <driver-name> [--max N] [--seed S] [vm/run.sh options]" >&2; exit 2; }
NAME="$1"
shift
MAX=20
SEED=1
while [ $# -gt 0 ]; do
	case "$1" in
	--max) MAX="${2:?--max needs a number}"; shift 2 ;;
	--seed) SEED="${2:?--seed needs a number}"; shift 2 ;;
	*) break ;;
	esac
done

# Anything left goes to vm/run.sh, after the class's own VM options.
EXTRA=("$@")

. "$ROOT/harness/classes.sh"
driver_class "$NAME" || { echo "MUTANTS driver=$NAME result=fail: not listed in harness/classes.sh"; exit 2; }
DIR="$ROOT/drivers/$NAME"
SRC="$DIR/src/lib.rs"
[ -f "$SRC" ] || { echo "MUTANTS driver=$NAME result=fail: $SRC missing"; exit 2; }
RUN="$ROOT/harness/runs/$NAME/mutants-$(date -u +%Y%m%dT%H%M%SZ)"
WORK="$ROOT/target/mutants/$NAME"
mkdir -p "$RUN" "$WORK"
# Only the driver builds go to $WORK/build. It is not exported: the
# conformance test builds grind-host (and its helpers) into $ROOT/target and
# boots the VM with that binary, so a target dir set here would leave it
# running a stale host.
DRIVER_TARGET="$WORK/build"
WIT="$ROOT/portals/wit"

# Build a copy of the driver from source file $1 into $2/driver.wasm.
build() {
	local src="$1" out="$2" tree="$WORK/tree"
	rm -rf "$tree" && mkdir -p "$tree/src"
	cp "$DIR/Cargo.toml" "$DIR/Cargo.lock" "$tree/"
	# The copy lives elsewhere, so point wit-bindgen at the WIT directly.
	sed "s|path: \"../../portals/wit\"|path: \"$WIT\"|" "$src" >"$tree/src/lib.rs"
	CARGO_TARGET_DIR="$DRIVER_TARGET" cargo build --release --locked --offline --manifest-path "$tree/Cargo.toml" \
		--target wasm32-unknown-unknown >"$out/build.log" 2>&1 || return 1
	cp "$(find "$DRIVER_TARGET/wasm32-unknown-unknown/release" -maxdepth 1 -name '*.wasm' | head -n 1)" "$out/driver.wasm"
}

conformance() { # out-dir: exit status of the test
	"$ROOT/tests/$CLASS/check.sh" "$1/driver.wasm" "${VMARGS[@]}" "${EXTRA[@]}" >"$1/test.log" 2>&1
}

# 1. Baseline.
mkdir -p "$RUN/baseline"
if ! build "$SRC" "$RUN/baseline"; then
	echo "MUTANTS driver=$NAME result=fail: the unmutated driver does not build (see $RUN/baseline/build.log)"
	exit 1
fi
if ! conformance "$RUN/baseline"; then
	echo "MUTANTS driver=$NAME result=fail: the unmutated driver fails tests/$CLASS (see $RUN/baseline/test.log); a score would mean nothing"
	exit 1
fi
echo "MUTANT baseline result=pass (tests/$CLASS)"
# Record which host the runs used, so a stale binary would show.
HOST_BIN="$ROOT/target/x86_64-unknown-linux-gnu/release/grind-host"
if [ -f "$HOST_BIN" ]; then
	echo "MUTANT host sha256=$(sha256sum "$HOST_BIN" | cut -c1-16) built=$(date -u -r "$HOST_BIN" +%Y-%m-%dT%H:%M:%SZ)"
fi

# 2. The sample.
python3 -I "$ROOT/harness/mutate.py" list "$SRC" >"$RUN/all.tsv"
total=$(wc -l <"$RUN/all.tsv")
python3 -I -c 'import random,sys; n,k,s=int(sys.argv[1]),int(sys.argv[2]),int(sys.argv[3]); r=random.Random(s); print("\n".join(map(str,sorted(r.sample(range(n),min(k,n))))))' \
	"$total" "$MAX" "$SEED" >"$RUN/sample.txt"
echo "MUTANT sample size=$(grep -c . "$RUN/sample.txt") of=$total seed=$SEED"

# 3. Each mutant.
caught=0 survived=0 unbuildable=0
: >"$RUN/survivors.txt"
while read -r i; do
	[ -n "$i" ] || continue
	desc="$(awk -F'\t' -v i="$i" '$1==i {print "line " $2 ": " $3 ": " $4 " => " $6}' "$RUN/all.tsv")"
	out="$RUN/m$i"
	mkdir -p "$out"
	python3 -I "$ROOT/harness/mutate.py" apply "$SRC" "$i" "$out/lib.rs"
	if ! build "$out/lib.rs" "$out"; then
		verdict=unbuildable
		unbuildable=$((unbuildable + 1))
	elif conformance "$out"; then
		verdict=survived
		survived=$((survived + 1))
		echo "m$i $desc" >>"$RUN/survivors.txt"
	else
		verdict=caught
		caught=$((caught + 1))
		why="$(grep -aE 'step=[a-z0-9-]+ result=fail' "$out/test.log" | head -n 1 | cut -c1-160)"
		desc="$desc | first failure: ${why:-see test.log}"
	fi
	echo "MUTANT m$i result=$verdict $desc" | tr -d '\r' | cut -c1-400
done <"$RUN/sample.txt"

counted=$((caught + survived))
echo "MUTANTS driver=$NAME caught=$caught survived=$survived unbuildable=$unbuildable score=$caught/$counted run=${RUN#"$ROOT"/}"
if [ "$survived" -gt 0 ]; then
	echo "Survivors (each a gap in tests/$CLASS, or a mutation that changes nothing):"
	sed 's/^/  /' "$RUN/survivors.txt"
	exit 1
fi
exit 0
