<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# grind-host

Runs one Wasm driver against one device, through portals only. Trusted.

```text
grind-host --device <pci-address> --module <driver.wasm>
           [--timeout-ms N]           default 10000
           [--dma-limit-mib N]        default 64
           [--trace]                  log every register access and DMA allocation
           [--allow-unreviewed-deny-list]   emulated devices only (rule 10)
```

1. **Module checks, before the device is touched.** The module must be a core
   module whose imports all come from the portal interfaces. Anything else
   (WASI included) is rejected and the rejection names the import. It is
   then wrapped as a component (`wit-component`) and compiled.
2. **Device.** `grind_shim::Device::open`.
3. **Run.** The driver is instantiated with only the portals linked, a 64 MiB
   memory limit and a wall-clock time limit (epoch interruption), and its
   `start` is called.

Output is JSON lines on stdout. The last line is always
`{"event":"result","status":"pass"|"fail","reason":...}`. Traps include a
Wasm backtrace with function names.

| Exit | Meaning |
| --- | --- |
| 0 | `start` returned ok and no denied write was attempted |
| 1 | the driver failed: `driver-error`, `trap`, `timeout` or `deny-list-violation` |
| 2 | the run could not start: arguments, module rejected, device not opened |

Dependencies are pinned exactly: `wasmtime` 50.0.1 (no WASI, no default
features), and `wasmparser` and `wit-component` 0.261.0, the version
Wasmtime 50 uses.
