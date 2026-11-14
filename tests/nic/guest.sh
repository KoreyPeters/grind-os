#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Runs inside the test VM (vm/init: grind.run=/tests/nic/guest.sh).
# Runs the NIC driver under grind-host, which answers ARP and ping for
# 10.0.2.15 through it. The peer on the other end of the wire (nic-peer,
# outside the VM) judges whether the answers arrived.
#
# Prints the host's JSON log indented, then
#   NIC step=host result=pass|fail expected="..." got="..."
# Exits with grind-host's exit status.

BDF="$1"
MODULE=/drivers/driver.wasm
IP=10.0.2.15
SERVE_MS=15000

clean() { printf '%s' "$*" | tr '\n"' "  "; }
expected="grind-host exits 0 after serving $IP for $SERVE_MS ms"

if [ ! -f "$MODULE" ]; then
	printf 'NIC step=host result=fail expected="%s" got="%s"\n' "$expected" "$MODULE missing from the initramfs"
	exit 1
fi

# The e1000e deny list is unreviewed (rule 10): allowed for emulated devices only.
out="$(/bin/grind-host --device "$BDF" --module "$MODULE" --allow-unreviewed-deny-list \
	--timeout-ms 15000 --serve-ping "$IP" --serve-ms "$SERVE_MS" 2>&1)"
rc=$?
printf '%s\n' "$out" | sed 's/^/    /'
last="$(printf '%s\n' "$out" | tail -n 1)"
[ "$rc" -eq 0 ] && result=pass || result=fail
printf 'NIC step=host result=%s expected="%s" got="%s"\n' "$result" "$expected" "$(clean "exit $rc; last line: $last")"
exit "$rc"
