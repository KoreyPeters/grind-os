// SPDX-License-Identifier: AGPL-3.0-or-later

//! A 32-bit write to MSR's dword (BAR1 + 0x58), which also covers CONFIG3
//! and CONFIG4. Expected: denied, naming CONFIG3, because a write is
//! checked against every byte it touches.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::mmio;

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        match mmio::write32(0x58, 0) {
            Err(mmio::Error::Denied) => Ok(()),
            other => Err(format!(
                "write32(0x58): expected Err(Denied), got {other:?}"
            )),
        }
    }
}

export!(Module);
