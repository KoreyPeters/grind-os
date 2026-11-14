// SPDX-License-Identifier: AGPL-3.0-or-later

//! Writes CC (0x14) to disable the controller, which belongs to the shim.
//! Expected: `denied`.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        match mmio::write32(0x14, 0) {
            Err(mmio::Error::Denied) => Ok(()),
            other => Err(format!("write32(CC 0x14): expected Err(Denied), got {other:?}")),
        }
    }
}

export!(Module);
