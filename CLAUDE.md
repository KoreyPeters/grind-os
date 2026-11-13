# Grind OS: instructions for agents

Read this before doing anything in this repository. These rules exist because the whole project is a bet that AI-written code can be trusted when the architecture contains it, tests prove it, and mistakes are reversible. Working around the rules defeats the point of the project.

## Current phase: Phase 0

Prove the bet before writing a kernel: an agent can grind a working WebAssembly driver that reaches hardware only through capability portals, using readable failures.

Phase 0 runs on Linux using VFIO (userspace device access with IOMMU-protected DMA), inside a QEMU virtual machine with an emulated Intel e1000e network card. The same driver later moves to real hardware: the Intel i219 in a Dell OptiPlex 7050 Micro.

### Milestones

1. **Test VM.** QEMU guest (q35 machine, emulated Intel IOMMU, emulated e1000e) boots; inside it, the e1000e device is unbound from Linux's driver and bound to `vfio-pci`.
2. **Hello hardware.** A tiny native Rust program in the guest opens the device through VFIO and reads one register.
3. **Shim and host.** A native bus shim over VFIO, and a Wasmtime host that exposes it to drivers as portals. The shim includes a per-device register deny list (rule 10).
4. **First grind.** A Wasm e1000e driver passes the first conformance test: bring the link up and answer a ping.

Work on one milestone at a time, in order.

## Repository layout

| Path | Contents | Trust |
| --- | --- | --- |
| `vm/` | Scripts to build and boot the test VM | Trusted |
| `harness/` | The grind loop: boot VM, load driver, run tests, collect logs | Trusted |
| `tests/` | Conformance tests per device class | Trusted |
| `portals/` | WIT interface definitions for portals | Trusted |
| `shim/` | Native Rust bus shim over VFIO | Trusted |
| `host/` | Wasmtime host exposing portals to drivers | Trusted |
| `drivers/<name>/` | Wasm drivers | Grindable |

Create directories as milestones need them.

## Non-negotiable rules

1. **Trusted code is not changed while grinding.** When working on a driver, change only files under `drivers/<name>/`. If a trusted component seems wrong, stop and describe the problem; do not patch it. Trusted components are built only when the human asks for that milestone.
2. **Drivers touch hardware only through portals.** No `unsafe`, no host imports other than the portals in `portals/`, no WASI filesystem or network access.
3. **Every failure must be readable.** Never swallow an error. Traps, timeouts and failed register checks produce structured log entries saying what was attempted, what was expected and what happened.
4. **Tests define done.** A driver is done only when its conformance test passes through the harness. Never weaken, skip or edit a test to make it pass.
5. **Reversible steps.** Small commits; every commit builds. Never rewrite published history.
6. **Write from specs, not from Linux.** Linux drivers are GPL-2.0-only, which is incompatible with this project's AGPL-3.0 license. Do not copy or translate Linux driver code. Use public datasheets and observed device behavior, and write a short spec in `drivers/<name>/SPEC.md` before implementing.
7. **Record provenance.** Each driver has `drivers/<name>/PROVENANCE.md` listing the sources consulted and how they were used.
8. **License every file.** All source files start with an SPDX header: `SPDX-License-Identifier: AGPL-3.0-or-later`.
9. **Humans sign off.** Mark AI-assisted commits with an `Assisted-by:` trailer naming the model. The human reviews and signs off.
10. **Hardware safety lives in the shim.** The IOMMU stops stray memory access but not deliberate register writes. The shim keeps a per-device deny list of registers that could permanently damage hardware, such as those that write flash, NVM or firmware, identified from the device’s datasheet. Drivers never write non-volatile memory; the shim refuses and logs any attempt, and it counts as a test failure. No driver runs on real hardware until its device’s deny list exists and has been reviewed by the human.

## Environment

- Host: Windows 11 (AMD), WSL2 Ubuntu. The repository lives in the Linux home folder (`~/grind-os`), not under `/mnt/c`.
- KVM is available in WSL through nested virtualization; the `kvm_amd` module is loaded at boot.
- Toolchain: Rust (rustup), QEMU (`qemu-system-x86_64`), git, gh.
- Starting point for the VM, to be verified while building milestone 1: `-machine q35,accel=kvm,kernel-irqchip=split -device intel-iommu,intremap=on -device e1000e`, with `intel_iommu=on` on the guest kernel command line.
