<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Portal conformance (milestone 3)

```sh
vm/fetch.sh             # once
tests/portals/check.sh  # exit 0 only if everything passes
```

`check.sh` runs the shim's unit tests, builds `grind-host` (static) and the
test modules, then boots the VM and runs `guest.sh` against the e1000e.
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
| `unreviewed-refused` | no `--allow-unreviewed-deny-list` | exit 2 at `device-deny-list` |
| `denied-write` | writes a deny-listed register | exit 1, `mmio-denied` logged, `deny-list-violation` |

The modules in `modules/` are trusted test fixtures, not drivers. Two of
them use `unsafe` on purpose: `wasi-import` declares a foreign import, and
none of them are subject to the driver rules.

**Status:** every case passes except `denied-write`. Its module is not
written yet, because it needs a real entry in the e1000e deny list, which in
turn needs the 82574 datasheet (see `shim/README.md`).
