# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Which conformance test each driver must pass, and the VM options it
# needs. Sourced by harness/grind.sh and harness/mutants.sh. Kept in the
# trusted harness, not in the driver's directory, so a driver cannot choose
# its own test or device.
#
#   driver_class NAME   sets CLASS and VMARGS; returns 1 for an unknown
#                       driver.

driver_class() {
	VMARGS=()
	case "$1" in
	e1000e) CLASS=nic ;;
	nvme) CLASS=block ;;
	rtl8139) CLASS=nic VMARGS=(--nic rtl8139) ;;
	*) return 1 ;;
	esac
}
