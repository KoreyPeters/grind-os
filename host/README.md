<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# grind-host

Runs one Wasm driver against one device, through portals only. Trusted.

```text
grind-host --device <pci-address> --module <driver.wasm>
           [--timeout-ms N]           default 10000
           [--dma-limit-mib N]        default 64
           [--trace]                  log every register access and DMA allocation
           [--allow-unreviewed-deny-list]   emulators only: refused unless DMI sys_vendor is QEMU (rule 10)
           [--serve-ping <ipv4>]      NIC drivers: answer ARP and ping (world nic)
           [--serve-ms N]             how long to serve, default 10000
           [--exercise-block --expect-block-size N --expect-blocks N]
                                      block drivers: read, write, flush, read back (world block-driver)
```

1. **Module checks, before the device is touched.** The module must be a core
   module whose imports all come from the portal interfaces. Anything else
   (WASI included) is rejected and the rejection names the import. It is
   then wrapped as a component (`wit-component`) and compiled.
2. **Device.** `grind_shim::Device::open`. For an NVMe controller this
   includes the shim's controller bring-up.
3. **Run.** The world follows from the device and the options: `driver`;
   `nic` with `--serve-ping`; `nvme-driver` (adds the `nvme` portal) for a
   device that takes commands. A module that imports `nvme` is rejected on
   any other device. The driver is instantiated with only the portals linked and a
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

   The host polls `link-up` every 100 ms and logs each change. It stops
   when the peer sends a control frame saying its timeline is done
   (EtherType 0x88B5, `src/net.rs`), or after `--serve-ms`. To keep a
   flood from stalling the serial console, routine events are logged for
   the first 20 of each kind and then only counted (`serve-end`).

   The verdict follows the NIC robustness contract
   (`docs/design/robust-nic.md`): a driver error is logged, not fatal
   (reporting "link down" is correct). Serving fails on a trap, a
   timeout, an empty error (`driver-error-unreadable`), a frame outside
   14-1514 bytes (`frame-invalid`), DMA memory growing after `start`
   (`dma-growth`), or fewer link drops reported than
   `--expect-link-flaps` (`link-flaps-unseen`) or the link not up at the
   end (`link-stuck-down`). If the peer's "done" says it stopped early at
   a failed step, the host reports `peer-ended-early` instead of judging
   steps that never ran.

   **Fault injection** (`src/faults.rs`): a control frame from the peer
   opens a fault window (length, kinds, rate). During it, some register
   reads return all-ones, some DMA buffer reads come back with one byte
   changed, and some calls are slowed by 2 ms; the host closes the window
   itself. `--fail-allocation N` fails the driver's Nth DMA allocation.
   Faults come from a xorshift64 generator seeded by `--fault-seed`
   (default 1). Every fault is logged (`fault-injected`) with `t_ms`, the
   time since the run started, on the same clock as the driver's own log
   lines (`driver-log` now carries `t_ms` too), so a fault lines up with
   what the driver did next. A window that hits every register read (a
   device gone) is counted per register instead of one line per read.
   Each window ends with `fault-window-end`: its counts, and register
   faults by offset. Totals are in `serve-end`. Only what the driver
   sees changes: writes and anything reaching the device are untouched,
   so rule 10 holds. Whether frames were
   answered, and whether the driver recovered after each hostile phase,
   is for the test's peer to judge (`tests/nic`).
5. **Exercise** (only with `--exercise-block`). The device must take NVMe
   commands and the driver must target world `block-driver`. The host
   checks `block-size` and `capacity` against the expected values, then
   reads the first block, the last block, a 40-block and a 256-block run
   and compares them with the seed pattern, writes the data pattern there,
   flushes, and reads it back. Then it checks the error contract: seven
   reads and writes that do not fit, or are empty or partial, must each
   return a non-empty error, and the last and first blocks must read back
   unchanged afterwards (`src/block.rs`). Each call has 1 s. A wrong answer
   is `block-mismatch`, `block-geometry`, `block-accepted-invalid` or
   `block-error-unreadable`, logged with what was expected.
   Whether the data really reached the disk is for the test to judge from
   the image (`tests/block`).
6. **Teardown.** The host calls `Device::quiesce` to turn bus mastering
   off, and only then drops the driver's DMA buffers (unmapping them).

Output is JSON lines on stdout. The last line is always
`{"event":"result","status":"pass"|"fail","reason":...}`. Traps include a
Wasm backtrace with function names.

| Exit | Meaning |
| --- | --- |
| 0 | `start` returned ok, no denied write was attempted and no command was refused |
| 1 | the driver failed (`driver-error`, `trap`, `timeout`, `link-down`, `block-geometry`, `block-mismatch`, `block-accepted-invalid`, `block-error-unreadable`, `deny-list-violation`, `command-refused`), or bus mastering could not be cleared at the end (`quiesce`) |
| 2 | the run could not start: arguments, module rejected, device not opened |

Dependencies are pinned exactly: `wasmtime` 50.0.1 (no WASI, no default
features), and `wasmparser` and `wit-component` 0.261.0, the version
Wasmtime 50 uses.
