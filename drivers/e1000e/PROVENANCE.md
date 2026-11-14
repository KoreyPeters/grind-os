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
