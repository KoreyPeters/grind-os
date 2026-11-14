<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Grind OS — Phase 0 code review

2026-11-13 · Reviewed at commit `4546b8a` (26 commits). Snapshot of the living review doc: https://claude.ai/artifact/VZ5ZwYFi2nuZ2p5QhxdBrb

## Verdict

The code matches the design and is safe for Phase 0 in the VM; two safety items should be fixed before any driver touches the OptiPlex.

- **Design: conforms.** Portals are the only path to hardware, enforced three separate ways. Native shim and host, Wasm driver, exactly as designed.
- **Safety: strong, with two gaps.** The unreviewed-list escape hatch is an honor system, and the shim doesn't quiesce the device between runs. Neither matters in the VM; both matter on real hardware.
- **Quality: high.** All 22 native unit tests pass, every `unsafe` block is confined to the shim with a safety comment, and no function trips Clippy's complexity lint. Formatting and a few long functions need tidying.

## Design conformance

| Design element | In the code | Status |
| --- | --- | --- |
| Portals are the only path to hardware | `portals/wit/portals.wit` defines mmio, dma, clock, log; drivers can't name another device, see host pointers or reach config space | Conforms |
| Enforced, not trusted | Three layers: the host rejects any non-portal import before the device is opened; the linker contains only portals; Wasmtime is built without WASI | Conforms |
| Native shim, trusted | `shim/`: VFIO, register window, deny list, DMA. All 9 `unsafe` blocks in the project live here, each with a safety comment | Conforms |
| Wasm drivers, grindable | `drivers/e1000e/`: no `unsafe`, single dependency (wit-bindgen), checked by the harness on every run | Conforms |
| Memory objects (from JX) | DMA buffers are WIT resources: the driver holds a handle and an IOVA, never a pointer; dropping the handle unmaps the buffer | Conforms (no subranges yet) |
| Per-domain limits (from JX) | 64 MiB linear memory, a DMA allowance, and a per-call time limit via epoch interruption | Conforms |
| Only BAR0 exposed | Other BARs, I/O ports, MSI-X and config space stay in the shim | Conforms |
| Interrupts, verified handlers | Not built; the driver polls | Deferred, fine for Phase 0 |

## Safety gaps

1. **The unreviewed-list escape hatch is an honor system.** `--allow-unreviewed-deny-list` is documented as "emulated devices only", but nothing checks that.
2. **The device isn't quiesced between runs.** The shim enables bus mastering on open and never explicitly resets the device or disables DMA on close.
3. **A deny list, not an allow list.** Anything not listed is writable, including reserved or undocumented registers.
4. **Rule 1 isn't enforced.** The harness records `git status` but doesn't fail when trusted files changed during a grind.

## Findings backlog

Changes to trusted code: the human requests them; the agent does not make them while grinding.

| # | Severity | Finding | Fix | Where |
| --- | --- | --- | --- | --- |
| 1 | High | `--allow-unreviewed-deny-list` is honored on any machine | Honor it only when a hypervisor is detected (for example, the DMI system vendor reads QEMU); add a portal test that it's refused otherwise | shim/src/deny.rs, device.rs |
| 2 | High | The device isn't explicitly quiesced between runs | Reset the device at open (VFIO device reset) and clear bus mastering before unmapping DMA at close | shim/src/device.rs, dma.rs |
| 3 | High | No deny list for the OptiPlex's i219 yet | Write the i219-LM list from Intel's datasheet and have the human review it; until then the gate refuses the device | shim/src/deny.rs |
| 4 | Medium | The harness records but doesn't enforce rule 1 | Fail the grind if anything outside `drivers/<name>/` changed | harness/grind.sh |
| 5 | Medium | Unlisted registers, including reserved ones, are writable | For real hardware, also deny reserved and undocumented ranges, or move to per-device allow lists | shim/src/deny.rs |
| 6 | Medium | The NIC test still passes the unreviewed flag, though the 82574L list is now reviewed | Drop the flag so conformance proves the reviewed path | tests/nic/guest.sh |
| 7 | Low | 36 formatting differences in 4 files | Run `cargo fmt` and add `cargo fmt --check` to the harness | host, shim, tests/nic/peer |
| 8 | Low | 7 default Clippy warnings, including a `drop(instance)` that does nothing | Fix the 6 needless references; remove the no-op drop and correct its comment | host/src/main.rs, tests/nic/peer |
| 9 | Low | A few long functions | Split the driver's `start` into steps, `Device::open` into stages, and host/src/main.rs into modules | driver, shim, host |
| 10 | Low | A host-side sleep can overrun a call's deadline by up to 1 s | Document it, or check the deadline after each sleep | host/src/main.rs |
| 11 | Low | DMA copies run one byte at a time | Switch to bulk volatile copies when throughput matters | shim/src/dma.rs |

## To confirm by running

- `harness/grind.sh e1000e` passes end to end.
- `tests/portals/check.sh` passes, including the adversarial modules and the deny-list gate case.
- After a run, `dmesg` in the VM shows whether the IOMMU logged DMA faults at teardown (evidence for finding 2).
