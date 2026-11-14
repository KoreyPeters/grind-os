// SPDX-License-Identifier: AGPL-3.0-or-later

//! Writes I/O completion queue 1's head doorbell (0x100C), which belongs
//! to the shim: it consumes completions itself. Expected: `denied`.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        match mmio::write32(0x100C, 1) {
            Err(mmio::Error::Denied) => Ok(()),
            other => Err(format!("write32(CQ 1 head 0x100c): expected Err(Denied), got {other:?}")),
        }
    }
}

export!(Module);
