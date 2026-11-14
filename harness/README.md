<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

# Harness

```sh
harness/grind.sh e1000e
```

One grind iteration: check the rules, build the driver, run its
conformance test, keep the evidence. Trusted.

1. **Rule checks** on `drivers/<name>/`:
   - `SPEC.md` and `PROVENANCE.md` exist (rules 6, 7);
   - every `.rs`, `.toml`, `.md`, `.sh` and `.wit` file has the SPDX header
     (rule 8);
   - no `unsafe` in driver source (rule 2);
   - no build script, and `wit-bindgen` is the only dependency, so nothing
     from the driver runs on this machine at build time.
2. **Build** for `wasm32-unknown-unknown` with `--locked`.
3. **Conformance**: the test for the driver's device class. The mapping
   lives in `grind.sh` (`e1000e` → `tests/nic`), not in the driver's
   directory, so a driver cannot pick its own test.
4. **Evidence** in `harness/runs/<name>/<UTC time>/` (gitignored):
   `output.log`, `build.log`, `driver.wasm`, `serial.log`, `peer.log`, and
   the commit and working-tree status the run used.

The final line is `GRIND driver=<name> result=pass|fail run=<dir>`. Exit 0
only if everything passed. Extra arguments go to `vm/run.sh`.

The host enforces the rest at run time: portal-only imports (rule 2), the
deny list (rule 10), time limits, and readable failures (rule 3).
