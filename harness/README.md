<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Harness

```sh
harness/grind.sh e1000e
```

One grind iteration: check the rules, build the driver, run its
conformance test, keep the evidence. Trusted.

1. **Rule checks:**
   - nothing outside `drivers/<name>/` has changed (rule 1): any
     uncommitted or untracked change elsewhere in the repository, including
     trusted code, tests and this harness, fails the grind and is listed.
     Ignored files (`target/`, `harness/runs/`) don't count. Changes
     already committed are not checked; review those in history;
   - `SPEC.md` and `PROVENANCE.md` exist (rules 6, 7);
   - every `.rs`, `.toml`, `.md`, `.sh` and `.wit` file has the SPDX header
     (rule 8);
   - no `unsafe` in driver source (rule 2);
   - no build script, and `wit-bindgen` is the only dependency, so nothing
     from the driver runs on this machine at build time.
2. **Build** for `wasm32-unknown-unknown` with `--locked`.
3. **Conformance**: the test for the driver's device class. The mapping
   lives in `classes.sh` (`e1000e` → `tests/nic`, `nvme` → `tests/block`,
   `rtl8139` → `tests/nic` with `vm/run.sh --nic rtl8139`), not in the
   driver's directory, so a driver cannot pick its own test or device.
4. **Evidence** in `harness/runs/<name>/<UTC time>/` (gitignored):
   `output.log`, `build.log`, `driver.wasm`, `serial.log`, `peer.log` (NIC
   only), and the commit and working-tree status the run used. The block
   test's image check is in `output.log`; the image itself stays in
   `target/block-test/disk.img` until the next run.

The final line is `GRIND driver=<name> result=pass|fail run=<dir>`. Exit 0
only if everything passed. Extra arguments go to `vm/run.sh`.

The host enforces the rest at run time: portal-only imports (rule 2), the
deny list (rule 10), time limits, and readable failures (rule 3).

## Mutation score

```sh
harness/mutants.sh nvme --max 20 --seed 1
```

How strong is a conformance test? `mutants.sh` breaks a passing driver on
purpose and counts how many of the breaks its test catches
(`docs/design/robust-nic.md`, part 3). `mutate.py` lists every mutation
of `drivers/<name>/src/lib.rs`, one typical mistake at one place: a
comparison boundary moved, `==`/`!=` or `&&`/`||` flipped, a `+ 1` or
`- 1` dropped, a register write removed, a wait shortened to nothing.
String literals, comments and test code are left alone. A seeded sample
is built (in `target/mutants/<name>/`) and run through the driver's
conformance test: **caught** if the test fails, **survived** if it
passes, **unbuildable** if it does not compile (not counted). The
unmodified driver must pass first, or there is no score.

The output ends with `MUTANTS driver=<name> caught=C survived=S
unbuildable=U score=C/(C+S)` and lists every survivor: a gap in the test,
or a mutation that changes nothing the test can see. Logs, each mutant's
source and test output are kept in
`harness/runs/<name>/mutants-<UTC time>/`. A mutant takes a VM run, so
this is a separate command, run when a test or driver changes.

First result (2026-11-14), `nvme` against `tests/block`, 8 mutants:
3 caught, 5 survived. Survivors: a weakened namespace-format check and a
weakened completion-ID check (nothing feeds the driver a bad completion
or format: the block test has no fault injection yet), and three that
QEMU cannot tell apart (a queue size capped anyway, a timeout moved by
1 ns, and refusing a one-namespace device while QEMU reports several).

