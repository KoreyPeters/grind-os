<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Design: robust NIC drivers, forced by the test

Status: **approved by the human on 2026-11-14**, including the six decisions below (mutants: our own small mutator, not cargo-mutants).

## Why

Every driver so far passed on its first harness run. The passes are real
(each is judged by something outside the driver), but they say little:
the tests checked the normal path in a forgiving emulator, and grinding
optimises for exactly what the test checks (rule 4: tests define done).
Nothing has required a driver to cope with a link that drops, a flood of
frames, hostile frames, a device that stops answering, or a device that
reports nonsense. So nobody knows how these drivers behave then.

The fix is to define robustness and then **force the conditions in every
run**, whether or not a real environment would produce them at that
moment. This architecture is well placed for it: every register access,
every DMA read and every frame already passes through code we control
(shim, host, `nic-peer`), so faults can be injected there, deterministically
and reproducibly, and the driver cannot tell them from real ones.

Scope: the NIC class first (two chips, e1000e and RTL8139, to compare).
The same pattern then applies to `tests/block`.

## 1. The NIC robustness contract

Added to the `ethernet` interface's docs in `portals/wit` (as the block
error contract was), and every NIC driver's `SPEC.md` must say how it
meets each item:

| Condition | Required behaviour |
| --- | --- |
| Link goes down | `link-up` returns false within 1 s. `transmit` may return an error ("link down") or drop the frame. No trap, no hang. |
| Link comes back | `link-up` true and frames flow again within 5 s, without the host calling `start` again. |
| Receive overflow (frames arrive faster than the host polls) | Frames may be lost, but the driver detects the condition and recovers without restarting: frames flow again within 1 s of the flood ending. |
| Hostile frames (oversized, runt, garbage) | Never returned to the host if outside 14-1514 bytes; never break later receives. |
| Transmit ring full | `transmit` returns an error, never blocks past the call limit. |
| Device reads all-ones (gone or wedged) | Every call returns within the call limit with a readable error; no unbounded wait. Recovers if the device comes back. |
| Device writes nonsense into DMA memory (a bad descriptor length, status or index) | Validated before use; a readable error or the frame dropped; never an out-of-range access, a trap or a wrong frame handed up. |
| DMA allocation fails | `start` returns a readable error; nothing leaks. |
| Over a whole run | No trap, no timeout, no growth in DMA memory after `start`, every error message non-empty. |

Errors become legitimate: a driver that reports "link down" correctly
must not fail the run for it (today any driver error fails the run).

## 2. Forcing the conditions

A run becomes a scripted **timeline of phases**, driven by `nic-peer`
(outside the VM, where the oracle already lives). After every hostile
phase comes a **recovery check**: 20 pings that must all succeed, the
first within 5 s. A driver passes only if it recovers from everything.

| Phase | How | Recovery required |
| --- | --- | --- |
| baseline | ARP, 3 pings, 300-ping bulk (today's test) | |
| flood | 2000 frames back to back, mixed sizes, without waiting for replies | yes |
| hostile frames | oversized (up to 9000 bytes), runts, unknown EtherTypes, truncated IPv4/ICMP, bad checksums | yes |
| link flap | the peer drops the link through QEMU's monitor (QMP `set_link`), sends pings that must go unanswered, raises the link, three times | yes |
| device faults | for a window, the host injects faults (below) | yes |
| final | bulk again | |

**Device faults** (in the host, the only place that sees every portal
call), driven by a seed so a failing run replays exactly:

- **register reads:** for a window, some `mmio` reads return all-ones
  (what a removed or wedged PCIe device returns);
- **device data:** some `dma` buffer reads come back with bytes changed,
  which looks to the driver like a device writing nonsense into a
  descriptor or header;
- **allocation:** one run in N has `dma.allocate` fail during `start`;
- **time:** calls are slowed and the clock jumps, within the limits.

Faults are bounded (a window, a rate) and logged (`fault-injected`, with
the seed, what and where), so every failure is readable and reproducible.
The register and DMA faults never reach the device: they change what the
*driver* sees, not what the shim writes, so rule 10 is untouched.

The peer tells the host when a fault window starts with a control frame
on a private EtherType (0x88B5, reserved for local experiments). The host
recognises it, never hands it to the responder, and ends the window
itself after its stated length, so it never depends on receiving another
frame while faults are on.

**Verdict.** The host no longer fails a run on the first driver error.
It fails a run on a trap, a timeout, an empty error message, a frame
outside 14-1514 bytes, or DMA memory growing after `start`. Everything
else is judged by the peer: did each recovery check pass.

## 3. Measuring test strength: mutants

Today's suspicion was that the tests are too easy. Measure it.
`harness/mutants.sh <driver>` builds variants of a passing driver, each
with one deliberate mistake, runs the conformance test on each, and
reports how many were caught (the **mutation score**), listing the
survivors. Mutations: off-by-one on ring indexes and lengths, a removed
bounds check, a removed wait or reset, a `>=` turned to `>`, an error
path that returns ok. A survivor is either a test gap (fix the test) or
dead code. Mutants take a VM run each, so this is a separate command,
run when a test or driver changes, not on every grind.

## 4. Then regrind

Run the existing e1000e and RTL8139 drivers against the new test. If
they fail, as expected, grind them (fresh sessions, as before) until
they pass, recording every failing run. That is the real test of the
grind loop, and the result is drivers worth more than today's.

## Decisions for the human

1. **The contract** in section 1: the conditions and the time limits
   (1 s, 5 s, 20 pings).
2. **Host policy:** driver errors stop failing the run by themselves;
   the verdict moves to traps, timeouts, empty errors, bad frame sizes,
   DMA growth, and the peer's recovery checks.
3. **QMP:** `vm/run.sh` gains a QEMU monitor socket so `nic-peer` can
   flap the link.
4. **The control frame** (EtherType 0x88B5) from the peer to the host to
   start a fault window.
5. **Mutants:** `cargo-mutants` (MIT/Apache-2.0, a widely used tool,
   installed on this machine only and never a driver dependency), or a
   small mutator written here in the harness. I recommend writing our
   own: a handful of targeted mutations a driver is likely to get wrong,
   no new tool in the trusted path.
6. **Order of work**, as separate commits, each with its negative
   controls:
   1. the contract in `portals/wit` and the host's new verdict;
   2. QMP and the link-flap phase;
   3. flood and hostile-frame phases in `nic-peer`;
   4. device fault injection in the host, with the control frame;
   5. `harness/mutants.sh`;
   6. regrinding the e1000e and RTL8139 drivers.
