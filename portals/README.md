<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Portals

`wit/portals.wit` defines everything a driver can import (package
`grind:portals@0.1.0`, world `driver` and the worlds built on it).
Trusted: drivers never change it.

| Interface | Gives the driver | Never gives |
| --- | --- | --- |
| `mmio` | 8-, 16- and 32-bit reads/writes to the device's memory register BAR (BAR0, or BAR1 for the RTL8139), bounds- and naturally-alignment-checked; every byte a write touches is checked against the write list (rule 10) | other BARs, I/O port windows, MSI-X table, config space |
| `dma` | zeroed, page-aligned buffers mapped into the device's IOMMU domain; the driver sees only each buffer's IOVA | host pointers, other memory |
| `clock` | monotonic time, sleeps of up to 1 s per call | wall-clock time |
| `log` | structured log entries | any other output |

A driver exports `start() -> result<_, string>`. Interrupts are not
exposed: drivers poll.

An Ethernet driver targets world `nic`, which adds the exported `ethernet`
interface: `mac-address`, `link-up`, `transmit(frame)` and
`receive() -> option<frame>`. Frames exclude the FCS. The host calls these
only after `start` succeeds, one call at a time, and polls `receive`. This
is the smallest interface milestone 4's ping test needs; queue depths,
offloads and batching wait until a test needs them.

An NVMe driver targets world `nvme-driver`, which adds the `nvme` portal
(CLAUDE.md rule 10). The shim owns the controller's configuration and
every queue with its doorbells, so the driver never writes a command where
the device fetches it:

- `admin(command)` runs one 64-byte admin command and returns its 16-byte
  completion. For Create I/O Completion Queue and Create I/O Submission
  Queue, PRP1 must be 0: the shim allocates every queue.
- `submit(sqid, command)` queues one I/O command; `queue-full` until
  completions are collected.
- `complete(cqid)` returns the new 16-byte completion entries from a
  completion queue, oldest first.

The shim checks every command against the device's command list and the
driver's own DMA buffers, then copies it into its own queue. A refused
command returns `refused`, is logged as `command-refused`, and fails the
run. Through `mmio` the driver can read registers but write none. The
host links `nvme` only for a device that takes
commands and rejects a module that imports it anywhere else.

World `block-driver` adds the exported `block` interface (`block-size`,
`capacity`, `read`, `write`, `flush`), which a block conformance test calls
after `start` succeeds.

The host binds portals to exactly one device when it instantiates a
driver, so there is nothing to name another device with.

Drivers build for `wasm32-unknown-unknown` with `wit-bindgen`, so there
is no WASI. The host rejects any module that imports anything other than
these interfaces.
