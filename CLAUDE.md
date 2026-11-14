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
5. **Second device class.** A Wasm NVMe driver for QEMU's emulated NVMe controller, backed by a scratch disk image, passes a block conformance test. It reports the block size and capacity, writes patterns and reads them back exactly (first block, last block and multi-block transfers), and flushes. Commands go through the shim (rule 10), and a refused command fails the run.

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
6. **Use only sources whose license is compatible.** This project is AGPL-3.0-or-later. A driver may be written from public datasheets; from observed device behavior, including the register and DMA activity of any driver running on the device; and from source code under a compatible license: BSD, MIT, Apache-2.0, GPL-2.0-or-later, GPL-3.0-or-later or AGPL-3.0-or-later. Code under GPL-2.0-only (including Linux's `e1000e`), proprietary code and material obtained under NDA must not be copied, translated or used as a source. Copied or adapted code keeps its original copyright notice and license header. Write a short spec in `drivers/<name>/SPEC.md`, saying where each fact came from, before implementing.
7. **Record provenance.** Each driver has `drivers/<name>/PROVENANCE.md` listing every source consulted and how it was used; so does each shim list, in `shim/README.md`. For a document, record its title, revision, where it was obtained and, where possible, a checksum. For code, record the project, file path, exact commit and license. For a trace, record the reference driver and its version, the device, and where the trace is kept. For each source, say how it was used: facts taken, code copied or adapted (naming the files it went into), or only read.
8. **License every file.** Every source file starts with an SPDX header. Files written for this project use `SPDX-License-Identifier: AGPL-3.0-or-later`. A file that contains copied or adapted code names the original license after it, as in `SPDX-License-Identifier: AGPL-3.0-or-later AND BSD-3-Clause`, and keeps the original copyright notice and license text below the header.
9. **Humans sign off.** Mark AI-assisted commits with an `Assisted-by:` trailer naming the model. The human reviews and signs off.
10. **Hardware safety lives in the shim.** The IOMMU stops stray memory access, but not deliberate register writes or commands the device fetches from memory. No driver may permanently damage hardware, for example by writing flash, NVM or firmware, or by saving settings that survive a reset, so every path that could do so goes through the shim and is checked there:
    - **Register writes.** For each device, the shim keeps a list that decides which register writes are refused. Each list is one of these:
      - **A deny list from the datasheet or specification**, when it documents every register that can reach non-volatile memory, including any vendor-specific range. Every listed write is refused.
      - **An allow list from the datasheet or specification**, when it leaves some registers undocumented (for example, a vendor-specific range): only documented registers that a driver needs in normal operation and that cannot reach non-volatile memory. Every other write is refused.
      - **An allow list from a reference driver**, when no document is good enough: only the writes a reference driver makes in normal operation, taken from its source (if rule 6 permits it) or from traces of it running on the device. Every other write is refused. Entries that can reach non-volatile memory are removed.
    - **Commands**, on devices that fetch commands from memory (for example, NVMe). Drivers never write where the device fetches commands. They submit commands through a portal, and the shim checks each one against a per-device allow list from the specification or, where rule 6 permits it, a reference driver's source: the opcodes, and any conditions on their fields that keep them safe (for example, a feature may be set but not saved). The shim then copies the command into its own queue, so the driver cannot change it after the check. Vendor-specific opcodes and every other command are refused. The queues the device fetches from, and the registers that control or ring them, belong to the shim and are refused on its write list. Queues the device only writes to, such as completion queues, may belong to the driver only if the shim never relies on what the driver reports from them. If the shim needs to know how far the device has read a queue it owns, it reads that from completions in its own memory.
    - **Device addresses.** If the shim keeps any memory of its own in the device's IOMMU domain, such as command queues, the device must not be able to reach it on a driver's behalf. The shim checks that every address a driver gives the device lies inside that driver's own DMA buffers, for the full length of the transfer. Structures the device reads to find more addresses (for example, NVMe PRP lists) are refused unless the shim checks them and copies them into its own memory, because a driver could change them after the check.

    Every entry cites its source. Drivers never write non-volatile memory or firmware; the shim refuses and logs any attempt, and it counts as a test failure. No driver runs on real hardware until its device's lists (registers and, where they apply, commands) exist and have been reviewed by the human. Where possible, back up the device's non-volatile memory first (for example, by dumping the SPI flash) so a mistake can be undone. The review records whether a backup exists, and if not, why the human accepted that. Erasing stored data is not hardware damage, but on real storage devices, conformance tests use only a disk or namespace set aside for testing.

## Environment

- Host: Windows 11 (AMD), WSL2 Ubuntu. The repository lives in the Linux home folder (`~/grind-os`), not under `/mnt/c`.
- KVM is available in WSL through nested virtualization; the `kvm_amd` module is loaded at boot.
- Toolchain: Rust (rustup), QEMU (`qemu-system-x86_64`), git, gh.
- Starting point for the VM, confirmed while building milestone 1: `-machine q35,accel=kvm,kernel-irqchip=split -device intel-iommu,intremap=on -device e1000e`, with `intel_iommu=on` on the guest kernel command line.
