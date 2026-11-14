<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Portal conformance (milestone 3)

```sh
vm/fetch.sh             # once
tests/portals/check.sh  # exit 0 only if everything passes
```

`check.sh` runs the shim's unit tests, builds `grind-host` (static) and the
test modules, then boots the VM three times: to run `guest.sh` against the
e1000e; with `vm/run.sh --nvme` and a fresh 16 MiB scratch image
(`target/portals-nvme.img`) to run `guest-nvme.sh` against QEMU's NVMe
controller; and with `vm/run.sh --nic rtl8139` to run `guest-rtl8139.sh`
against the RTL8139.
`guest.sh` runs every case and prints
`PORTALS step=<case> result=pass|fail expected="..." got="..."`, with the
host's JSON log indented underneath.

| Case | Module | Expected |
| --- | --- | --- |
| `read-status` | reads STATUS (BAR0 + 0x8) | exit 0, value logged |
| `bounds` | out-of-range and misaligned accesses | exit 0, each gets the right error |
| `dma` | two buffers: alignment, no overlap, zeroed, round trip, bounds, bad size | exit 0 |
| `wasi-rejected` | imports `wasi_snapshot_preview1::proc_exit` | exit 2, import named, device untouched |
| `timeout` | spins forever, 1 s limit | exit 1, `timeout` |
| `trap` | panics | exit 1, `trap` with backtrace |
| `deny-list-gate` | `read-status` without `--allow-unreviewed-deny-list` | follows the list's `reviewed` flag: exit 2 at `device-deny-list` while unreviewed, exit 0 with `deny_reviewed: true` once reviewed (the gate is unit-tested in `shim/src/deny.rs` for both states) |
| `emulator-detected` | `read-status` with the allow flag | exit 0, `device-open` reports `dmi_sys_vendor: QEMU` (the value the rule 10 gate checks) |
| `denied-write` | writes NVM word 0 through EEWR (0x102C), then "handles" the `denied` error | exit 1, `mmio-denied` logged, `deny-list-violation` |
| `reset-at-open` | `read-status` | exit 0, `device-open` reports `reset: VFIO_DEVICE_RESET ok` |
| `quiesce-at-close` | `dma` (allocates buffers) | exit 0, `device-quiesce` logged with bus mastering off before the buffers are unmapped |
| `dma32` | two `allocate32` buffers and one `allocate` | exit 0: the low buffers lie from 1 MiB, below 4 GiB, clear of the MSI window, a guard page apart, and round-trip data; the ordinary one is from 4 GiB up |
| `dma-regions` | (same module) | `dma-regions` logs QEMU's usable IOVA ranges and the regions chosen from them |
| `nvme-portal-on-nic` | `nvme-format` (imports `nvme`) | exit 2 at `module-imports`: the `nvme` portal exists only for a device that takes commands |
| `result-last` | (same run) | the `result` line is still the last line after a setup error |
| `no-dma-faults` | (after all cases) | `dmesg` has no Intel IOMMU DMA fault lines |

Against the NVMe controller (`guest-nvme.sh`). Each refusal case's module
handles the error and returns ok; the host must still fail the run.

| Case | Module | Expected |
| --- | --- | --- |
| `nvme-gate` | `nvme-smoke` without the allow flag | exit 0: the register and command lists are human-reviewed (`deny_reviewed`, `commands_reviewed` true) |
| `nvme-smoke` | Identify (serial `grind0001`), Set Features, create an I/O queue pair, write 8 blocks, read them back exactly, flush | exit 0, `nvme-ready` logged with `dstrd: 0` |
| `nvme-dma32` | a 16-block write and read back through a buffer from `allocate32`, then flush | exit 0: the device reaches IOVAs below 4 GiB, and the shim's address checks accept them |
| `nvme-queue-full` | 15 Flushes on a 16-entry queue without collecting, then one more | exit 0: the 16th gets `queue-full`; after collecting, submitting works |
| `nvme-format` | Format NVM (80h) | exit 1, `command-refused` (`check: opcode`) |
| `nvme-save` | Set Features with Save set | exit 1, `command-refused` (`check: feature`) |
| `nvme-vendor-admin` | vendor-specific admin opcode C0h | exit 1, `command-refused` (`check: opcode`) |
| `nvme-unlisted-io` | Write Zeroes (08h) | exit 1, `command-refused` (`check: opcode`) |
| `nvme-prp-shim` | a Read into IOVA 0x1_0000_0000, the shim's own admin queue | exit 1, `command-refused` (`check: address`) |
| `nvme-prp-list` | a 12 KiB Read, which needs a PRP list | exit 1, `command-refused` (`check: prp-list`) |
| `nvme-cq-in-driver` | creates a completion queue in its own buffer | exit 1, `command-refused` (`check: prp`): the shim allocates every queue |
| `nvme-sq-doorbell` | writes SQ 1's tail doorbell (0x1008) | exit 1, `mmio-denied`, `deny-list-violation` |
| `nvme-cq-doorbell` | writes CQ 1's head doorbell (0x100C) | exit 1, `mmio-denied`, `deny-list-violation` |
| `nvme-cc-write` | writes CC (0x14) | exit 1, `mmio-denied`, `deny-list-violation` |
| `nvme-quiesce` | `nvme-smoke` | `device-quiesce` logged |
| `nvme-no-dma-faults` | (after all cases) | no IOMMU DMA faults: refused commands never reached the device |

Against the RTL8139 (`guest-rtl8139.sh`), whose registers are in BAR1:

| Case | Module | Expected |
| --- | --- | --- |
| `rtl-gate` | `rtl-read-id` without the allow flag | exit 0: the list is human-reviewed (`deny_reviewed: true`) |
| `rtl-bar1` | reads IDR0-5 with `read8` | exit 0; `device-open` reports `bar: 1`, `bar_size: 0x100`; the MAC is `52:54:00:12:34:56` |
| `rtl-reset-at-open` | `rtl-read-id` | `reset: VFIO_DEVICE_RESET ok` |
| `rtl-widths` | `write16`/`read16` of IMR (0x3C) | exit 0, reads back `0x0005` |
| `rtl-9346cr` | `write8(0x50, 0x80)`: EEPROM programming mode | exit 1, `mmio-denied` naming 9346CR, `deny-list-violation` |
| `rtl-flash` | `write32(0xD4, SWRWEn)`: software flash access | exit 1, `mmio-denied` naming FLASH |
| `rtl-config-dword` | `write32(0x58)`: MSR's dword, which covers CONFIG3/4 | exit 1, `mmio-denied` naming CONFIG3 |
| `rtl-quiesce` | `rtl-read-id` | `device-quiesce` logged |
| `rtl-no-dma-faults` | (after all cases) | no IOMMU DMA faults |

The shim refuses an I/O-port BAR: with the RTL8139's list temporarily
pointed at BAR0, `grind-host` failed at `device-bar` ("an I/O-port BAR
(config 0x0000c001)"). That was a one-off check, not a standing case,
because it needs a wrong list.

The NVMe modules share `modules/nvme-common/common.rs`, which each includes
at its crate root (wit-bindgen 0.63 cannot re-export a world's export
macro from a library crate).
The modules in `modules/` are trusted test fixtures, not drivers. Two of
them use `unsafe` on purpose: `wasi-import` declares a foreign import, and
none of them are subject to the driver rules.

**Status:** the e1000e cases pass on QEMU 4.2.1 and 6.2.0; the NVMe and
RTL8139 cases pass on QEMU 6.2.0. The RTL8139 deny list is human
reviewed, and its cases run without `--allow-unreviewed-deny-list`. The e1000e deny list and the NVMe register and command
lists are human reviewed (see `shim/README.md`). The NVMe cases run without
`--allow-unreviewed-deny-list`.
