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
6. **Flood and hostile frames** (robustness contract). The peer sends
   2000 frames back to back (pings of varying size, ARP for other
   addresses, an unknown EtherType), then 20 rounds of hostile frames: 1
   and 13 bytes, a bare header, a 59-byte runt, 1515, 1600, 4000 and
   9000 bytes, IPv4 and ICMP with bad checksums, a truncated IPv4 packet,
   and ARP with a bad address length. A recovery check follows each. The
   host fails the run if the driver hands it a frame outside 14-1514
   bytes (`frame-invalid`).

   What QEMU lets through: it applies backpressure when a card's receive
   ring is full and drops what the UDP socket cannot hold, so of the
   2000 flood frames the e1000e received about 350 and the RTL8139 about
   100, and a true hardware receive overflow cannot be forced. Its card
   models also drop the short and oversized frames themselves; only the
   checksum and truncation cases reach the drivers. The size rule is
   still enforced by the host, and real hardware will exercise it more.
7. **Link flaps** (robustness contract, `docs/design/robust-nic.md`).
   Through QEMU's monitor (`vm/run.sh --qmp`, QMP `set_link`), the peer
   drops the link three times for 2 s. While it is down, 3 pings must go
   unanswered (otherwise the test is not really dropping it). After each,
   a **recovery check**: 20 pings, the first answered within 5 s, the
   rest within 2 s each. The link then stays up at least 5 s, the time
   the contract gives a driver to report it up again. The host, polling
   `link-up` every 100 ms, must see the link go down at least 3 times
   (`--expect-link-flaps 3`, else `link-flaps-unseen`) and up at the end
   (else `link-stuck-down`).
8. The peer sends the host a "done" control frame, which ends serving.
   It says whether the timeline completed; if the peer stopped at a
   failed step, the host reports `peer-ended-early` (the failing step is
   in the peer's log) rather than judging steps that never ran.

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
`modules/leaky` allocates a DMA buffer on every `receive`, and
`modules/oversized` hands up a 1515-byte frame: grind-host must fail
them with `driver-error-unreadable`, `dma-growth` and `frame-invalid`,
the host's side of the robustness contract (`docs/design/robust-nic.md`):

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

The link check was checked against a mutant of the RTL8139 driver whose
`link-up` always returns true: every ping and recovery passed, and the
host failed it with `link-flaps-unseen` ("0 times"). The e1000e and
RTL8139 drivers pass all three flaps; the e1000e reports the link up
about 0.6 s after it returns (QEMU's auto-negotiation delay), the RTL8139
within one 100 ms poll.

While building this, the first version of the flap phase re-dropped the
link as soon as the recovery pings passed. That was too soon for the
host's 100 ms polling (and for the e1000e's auto-negotiation), so a
correct driver failed with `link-flaps-unseen`: a test bug, fixed by
holding the link up 5 s.
