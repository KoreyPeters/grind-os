<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# NIC conformance (milestone 4): link up, answer a ping

```sh
vm/fetch.sh                                # once
tests/nic/check.sh <driver.wasm>           # exit 0 only if everything passes
harness/grind.sh e1000e                    # builds the driver and runs this
```

```text
 nic-peer (this machine)              test VM
 02:00:00:00:00:02 10.0.2.2           e1000e 52:54:00:12:34:56
        |                                 |
        +---- UDP datagram per frame -----+  (vm/run.sh --net-udp)
                                          |
                               Wasm driver (world nic)
                                          |
                               grind-host --serve-ping 10.0.2.15
                               (answers ARP and ICMP echo, net.rs)
```

1. `check.sh` builds `grind-host` (static) and `nic-peer`, and puts the
   host, `guest.sh` and the driver into the initramfs.
2. `nic-peer` starts first. It reserves the UDP ports for the wire, then
   sends ARP who-has 10.0.2.15 every 500 ms while the VM boots.
3. In the VM, `guest.sh` runs the driver under
   `grind-host --serve-ping 10.0.2.15 --serve-ms 15000`. After `start`, the
   host polls the driver for frames for 15 s and answers through it.
4. Once ARP is answered, the peer sends three ICMP echo requests (56 data
   bytes each) and waits up to 2 s for each reply.
5. Then the **bulk** step: 300 more echo requests, one at a time, with
   1000 to 1472 data bytes (frames of 1042 to 1514 bytes, sizes varying
   so packets land at shifting offsets), about 371 KB each way. That wraps
   every receive and transmit ring a driver is likely to use many times
   (an 8 KiB receive ring about 45 times, a 32-entry descriptor ring about
   9 times), so wrap-around is tested, not just the first few slots. The
   step stops at the first failure and names the ping, its size, and how
   much traffic had already passed.

**Pass** requires all of:

- the VM checks pass and `grind-host` exits 0: `start` returned ok,
  `link-up` was true, no trap, no timeout, no driver error and no denied
  register write;
- the ARP reply says 10.0.2.15 is-at 52:54:00:12:34:56, with the correct
  Ethernet and ARP fields;
- each echo reply has the right MACs, addresses, id and sequence number,
  valid IPv4 and ICMP checksums, and exactly the data that was sent, for
  the three small pings and all 300 bulk ones.

Output: the host's JSON log (indented), `NIC step=<step> result=pass|fail
expected="..." got="..."` for `host`, `arp`, `ping-1`…`ping-3`, `bulk` and `peer`,
and a final `NIC result=pass|fail`. The peer prints every frame it
receives (`rx:` lines). Logs: `vm/build/serial.log`,
`target/nic-test/peer.log`.

## Negative controls

`modules/silent` claims the link is up but never receives anything. The
test must fail on it, which shows it cannot pass without a working driver.
`modules/mute-error` returns an empty error from `receive`, and
`modules/leaky` allocates a DMA buffer on every `receive`: grind-host
must fail them with `driver-error-unreadable` and `dma-growth`, the
host's side of the robustness contract (`docs/design/robust-nic.md`):

```sh
cargo build --release --manifest-path tests/nic/modules/Cargo.toml --target wasm32-unknown-unknown
tests/nic/check.sh tests/nic/modules/target/wasm32-unknown-unknown/release/nic_test_silent.wasm
# NIC step=arp result=fail ... got="the VM powered off; no ARP reply after 34 requests ..."
```

Verified on QEMU 6.2.0.

The bulk step was checked against a mutant: the C-mode RTL8139 driver
from the grind experiment with only its receive-ring wrap changed to
8192 + 16 bytes. ARP and the three small pings passed, as they would
have under the old test; `bulk` failed at ping 8, after 8,813 bytes,
where the ring first wraps (`no reply`; the driver's own log named a bad
receive header at ring offset 1356). The e1000e driver and both RTL8139
drivers (C+ and C mode) pass the bulk step.
