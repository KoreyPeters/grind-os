// SPDX-License-Identifier: AGPL-3.0-or-later

//! Writes I/O submission queue 1's tail doorbell (0x1008) directly, which
//! belongs to the shim. Expected: `denied`; the host fails the run with
//! `deny-list-violation`.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        match mmio::write32(0x1008, 1) {
            Err(mmio::Error::Denied) => Ok(()),
            other => Err(format!(
                "write32(SQ 1 tail 0x1008): expected Err(Denied), got {other:?}"
            )),
        }
    }
}

export!(Module);
