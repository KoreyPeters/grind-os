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
# An upper bound: the peer ends serving with a control frame when its
# timeline is done.
SERVE_MS=60000
# The peer drops the link this many times (tests/nic/peer); the host fails
# the run if the driver reports fewer.
LINK_FLAPS=3

clean() { printf '%s' "$*" | tr '\n"' "  "; }
expected="grind-host exits 0 after serving $IP for $SERVE_MS ms"

if [ ! -f "$MODULE" ]; then
	printf 'NIC step=host result=fail expected="%s" got="%s"\n' "$expected" "$MODULE missing from the initramfs"
	exit 1
fi

# No --allow-unreviewed-deny-list: the driver must pass with the device's
# human-reviewed list (rule 10), the same path real hardware takes.
out="$(/bin/grind-host --device "$BDF" --module "$MODULE" \
	--timeout-ms 15000 --serve-ping "$IP" --serve-ms "$SERVE_MS" --expect-link-flaps "$LINK_FLAPS" 2>&1)"
rc=$?
printf '%s\n' "$out" | sed 's/^/    /'
last="$(printf '%s\n' "$out" | tail -n 1)"
[ "$rc" -eq 0 ] && result=pass || result=fail
printf 'NIC step=host result=%s expected="%s" got="%s"\n' "$result" "$expected" "$(clean "exit $rc; last line: $last")"
exit "$rc"
