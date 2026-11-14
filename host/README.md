<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# grind-host

Runs one Wasm driver against one device, through portals only. Trusted.

```text
grind-host --device <pci-address> --module <driver.wasm>
           [--timeout-ms N]           default 10000
           [--dma-limit-mib N]        default 64
           [--trace]                  log every register access and DMA allocation
           [--allow-unreviewed-deny-list]   emulated devices only (rule 10)
           [--serve-ping <ipv4>]      NIC drivers: answer ARP and ping (world nic)
           [--serve-ms N]             how long to serve, default 10000
```

1. **Module checks, before the device is touched.** The module must be a core
   module whose imports all come from the portal interfaces. Anything else
   (WASI included) is rejected and the rejection names the import. It is
   then wrapped as a component (`wit-component`) and compiled.
2. **Device.** `grind_shim::Device::open`.
3. **Run.** The driver is instantiated with only the portals linked and a
   64 MiB memory limit, and its `start` is called with `--timeout-ms` to
   finish (epoch interruption).
4. **Serve** (only with `--serve-ping`). The driver must target world
   `nic`. The host asks for its MAC address and link state (link down fails
   the run), then polls `receive` for `--serve-ms`. `src/net.rs` answers
   ARP requests and ICMP echo requests for the address, and the host sends
   the answers with `transmit`. It is a stand-in for a network stack,
   written from RFCs 826, 791, 792 and 1071. Each `ethernet` call has 1 s
   to return. Each frame is logged as `answered`, `frame-ignored` or
   `frame-malformed`; a frame outside 14–1514 bytes is a driver error.

   Serving ends in a pass if nothing failed. Whether the pings were
   actually answered on the wire is for the test's peer to judge
   (`tests/nic`).

Output is JSON lines on stdout. The last line is always
`{"event":"result","status":"pass"|"fail","reason":...}`. Traps include a
Wasm backtrace with function names.

| Exit | Meaning |
| --- | --- |
| 0 | `start` returned ok and no denied write was attempted |
| 1 | the driver failed: `driver-error`, `trap`, `timeout`, `link-down` or `deny-list-violation` |
| 2 | the run could not start: arguments, module rejected, device not opened |

Dependencies are pinned exactly: `wasmtime` 50.0.1 (no WASI, no default
features), and `wasmparser` and `wit-component` 0.261.0, the version
Wasmtime 50 uses.
