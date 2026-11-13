<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Portals

`wit/portals.wit` defines everything a driver can import (package
`grind:portals@0.1.0`, world `driver`). Trusted: drivers never change it.

| Interface | Gives the driver | Never gives |
| --- | --- | --- |
| `mmio` | 32-bit reads/writes to the device's BAR0, bounds- and alignment-checked; writes filtered by the deny list (rule 10) | other BARs, I/O port windows, MSI-X table, config space |
| `dma` | zeroed, page-aligned buffers mapped into the device's IOMMU domain; the driver sees only each buffer's IOVA | host pointers, other memory |
| `clock` | monotonic time, sleeps of up to 1 s per call | wall-clock time |
| `log` | structured log entries | any other output |

A driver exports `start() -> result<_, string>`. The packet-facing side
(sending and receiving frames) is deliberately left out until milestone 4's
ping test shows what it needs. Interrupts are not exposed: drivers poll.

The host binds portals to exactly one device when it instantiates a
driver, so there is nothing to name another device with.

Drivers build for `wasm32-unknown-unknown` with `wit-bindgen`, so there
is no WASI. The host rejects any module that imports anything other than
these four interfaces.
