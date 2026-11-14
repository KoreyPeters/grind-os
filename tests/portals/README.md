<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Portal conformance (milestone 3)

```sh
vm/fetch.sh             # once
tests/portals/check.sh  # exit 0 only if everything passes
```

`check.sh` runs the shim's unit tests, builds `grind-host` (static) and the
test modules, then boots the VM twice: once to run `guest.sh` against the
e1000e, and once with `vm/run.sh --nvme` and a fresh 16 MiB scratch image
(`target/portals-nvme.img`) to run `guest-nvme.sh` against QEMU's NVMe
controller.
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
| `nvme-portal-on-nic` | `nvme-format` (imports `nvme`) | exit 2 at `module-imports`: the `nvme` portal exists only for a device that takes commands |
| `result-last` | (same run) | the `result` line is still the last line after a setup error |
| `no-dma-faults` | (after all cases) | `dmesg` has no Intel IOMMU DMA fault lines |

Against the NVMe controller (`guest-nvme.sh`). Each refusal case's module
handles the error and returns ok; the host must still fail the run.

| Case | Module | Expected |
| --- | --- | --- |
| `nvme-gate` | `nvme-smoke` without the allow flag | exit 2 at `device-deny-list`: the register and command lists are not reviewed |
| `nvme-smoke` | Identify (serial `grind0001`), Set Features, create an I/O queue pair, write 8 blocks, read them back exactly, flush | exit 0, `nvme-ready` logged with `dstrd: 0` |
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

The NVMe modules share `modules/nvme-common/common.rs`, which each includes
at its crate root (wit-bindgen 0.63 cannot re-export a world's export
macro from a library crate).
The modules in `modules/` are trusted test fixtures, not drivers. Two of
them use `unsafe` on purpose: `wasi-import` declares a foreign import, and
none of them are subject to the driver rules.

**Status:** the e1000e cases pass on QEMU 4.2.1 and 6.2.0; the NVMe cases
pass on QEMU 6.2.0. The e1000e deny list is human reviewed (see
`shim/README.md`); the cases pass in either state. The NVMe lists are not
reviewed yet, so the NVMe cases use `--allow-unreviewed-deny-list`.
