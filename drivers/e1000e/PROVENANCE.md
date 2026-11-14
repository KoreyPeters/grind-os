<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Provenance: e1000e driver

## Sources consulted

- **Intel 82574 GbE Controller Family Datasheet, revision 2.5 (April
  2009).** The same file the shim's deny list cites (sha256
  `bca28652978d7024830c5a6dfad9105f118d5b9a5b540b1ffa994bfc7178bea2`,
  kept in the gitignored `.cache/datasheets/rs.pdf`). Used for: 4.6
  (software initialization sequence), 3.1.3.10 (master disable), 7.1.3 and
  7.1.8 (legacy RX descriptor, RX ring), 7.2.10.1 (legacy TX descriptor),
  and the register descriptions in 10.2.2–10.2.6 (CTRL, STATUS, FCAL, FCAH,
  FCT, GCR, ICR, IMC, RCTL, RDBAL/H, RDLEN, RDH, RDT, MTA, RAL, RAH, TCTL,
  TIPG, TDBAL/H, TDLEN, TDH, TDT, TXDCTL). Read as text with
  `pdftotext -layout`. One unit was checked against the raw glyph: the
  post-reset wait in 10.2.2.1 is 1 µs (Symbol-font U+F06D), not 1 s.
- **Intel Specification Update 320709 rev 4.1.** Already reviewed for the
  shim; it lists no documentation changes affecting these registers.

## Not consulted

- Linux `e1000e`, `e1000` or any other Linux driver source (GPL-2.0-only,
  rule 6).
- QEMU's `hw/net/e1000e*.c` device model. If the driver and QEMU disagree,
  observed behaviour is recorded here as an observation, not copied from
  QEMU source.

## How they were used

`SPEC.md` was written from the datasheet sections above; the driver is
written from `SPEC.md`. Register offsets, bit positions and descriptor
layouts are facts taken from the datasheet; no code was copied or
translated.

## Observations of device behaviour

- QEMU 4.2.1 and 6.2.0 `e1000e`: STATUS reads `0x00000281` after Linux's
  driver is unbound, seen in milestones 2 and 3. Decoded per 10.2.2.2:
  FD = 1, LU = 0, SPEED = 10b (1000 Mb/s), ASDV = 10b. The link is not up
  until the driver sets CTRL.SLU.
- QEMU 6.2.0 `e1000e`, running this driver: the reset completes within the
  first 1 ms poll. STATUS.LU sets about 270 ms after CTRL.SLU, and STATUS
  then reads `0x00080283` (FD, LU, SPEED = 1000 Mb/s, ASDV, and
  GIO_MASTER_ENABLE back on after the reset). DMA IOVAs start at
  `0x100000000`, so RDBAH and TDBAH are 1. Frames sent while `start` runs
  (before RCTL.EN) are not received.

## Robustness grind log

Regrind against the strengthened NIC test (`docs/design/robust-nic.md`,
`tests/nic/README.md`), 2026-11-14, in the worktree on branch
`experiment/regrind-e1000e`. Sources: the same datasheet as above, plus
these sections not used before: 7.1.3.1 (the device writes length,
checksum, status, errors and VLAN, not the address), 7.1.8 (write-back
before head advance; hardware owns head..tail), 7.2.4 (TDH counts
descriptors loaded into the transmit FIFO), 10.2.2.1 (CTRL.RST as the
last-ditch recovery from a hung state), 10.2.5.7–9 and 10.2.6.6–8
(reserved fields; RDH/TDH written only after reset). Specification Update
320709 rev 4.1 was searched again for descriptor, head, tail and hang
errata: none apply. No Linux driver code, no QEMU source, no web.

Runs are under `harness/runs/e1000e/`. Runs 2 onward were started through
a short symlink to the worktree (`/tmp/claude-1000/regrind-e1k`), see run 1.

1. **20261114T183946Z: fail** (environment, unchanged driver).
   `NIC step=arp`: "the VM powered off; no ARP reply". The serial log says
   QEMU refused `-qmp unix:<worktree>/target/nic-test/qmp.sock`: "UNIX
   socket path ... is too long. Path must be less than 108 bytes". The
   worktree path is long, and `tests/nic/check.sh` puts the QMP socket
   under it. Not a driver problem and not patched; worked around by
   running `harness/grind.sh` through a short symlink (its ROOT comes from
   `pwd`, so the socket path gets short). Local builds before it: 0.
2. **20261114T184556Z: fail** (unchanged driver, to confirm the known
   failure). `recover-after-faults-device-data`: "no reply within 5000 ms
   (10 tries)". Here it was the device-data window (window 2), not window
   3 as the test README recorded. The host log shows the cause: during the
   window, unfilled descriptors 20–25 read as done (status 0x97, 0xe9,
   0xd5, 0x85, 0x85, 0x87, all length 0); the driver dropped each, but
   recycled it and moved RDT onto it, until RDT reached the card's head
   (an empty ring) and receive stopped for good. `serve-end` also showed
   `frames_malformed: 18` against 10 hostile ones in later runs:
   corrupted payloads had been handed up. Change: the redesign below (two
   agreeing reads of descriptors and payloads, address check, RDH must
   have passed a descriptor before it is taken, TDH-based TX reclaim, RX
   and health checks with resync and re-initialisation, allocations
   before configuration). Local builds before run 3: 1 (it compiled first
   time).
3. **20261114T184735Z: pass.** Every step passed. `frames_malformed` was
   10, all the deliberate bad-checksum hostile frames, sent before the
   fault windows. But 6 non-fatal `receive`/`transmit` errors said
   "could not read RDH: expected two agreeing reads below 32, got 0x5,
   0xffffffff, 0x5, 0xffffffff, ...": the injected all-ones reads
   alternated with good ones, and `read_index` wanted two *consecutive*
   agreeing reads. Diagnosis: a needless error, not a test failure. Change:
   skip out-of-range reads instead of restarting the comparison, 12 tries;
   payload and descriptor tries 4 → 6. Local builds: 1.
4. **20261114T184851Z: pass.** No driver errors while serving
   (`driver_errors: 0`); 3 descriptor reads disagreed and were retried on
   the next call. The resync and re-initialisation paths never ran.
   Local builds: 0.
5. **20261114T184949Z: pass** (deliberate mutant, not the driver).
   `receive` changed to trust one descriptor read with no RDH check, to
   exercise the recovery path. The RX check logged "receive out of step
   with the device; resynchronising at RDH" 5 times, 4 of them the exact
   old deadlock (`rdh` = `rdt` = `rx_next` − 1, 31 skipped), and the run
   passed. Change reverted. Local builds: 2 (one compile error in the
   mutant).
6. **20261114T185103Z: pass** (deliberate mutant). One re-initialisation
   forced 20 s into serving; it ran inside one `receive` call just after
   the third link flap, and every later phase (three fault windows, final
   bulk) passed with no DMA growth. Change reverted. Local builds: 2
   (first trigger was call-count based and probably would not fire;
   switched to time).
7. **20261114T185253Z: pass** (final driver, SPEC.md and this log
   updated). `driver_errors: 0`, `frames_malformed: 10` (the hostile
   ones), DMA 139264 bytes after `start` and at the end. One frame was
   dropped because no two of its 6 buffer reads agreed. That is more
   often than independent 10% faults would predict, which suggests the
   injected faults are correlated (as the alternating all-ones in run 3
   also suggested). Local builds: 0.
8. **20261114T185331Z: pass** (same driver, to check run 7 was not luck).
   Same counts. Local builds: 0.

Totals: 8 harness runs (1 environment failure, 1 confirming the old
failure, 2 deliberate mutants, 4 passes of the real driver) and 6 local
builds.
