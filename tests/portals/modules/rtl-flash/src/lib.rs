// SPDX-License-Identifier: AGPL-3.0-or-later

//! Writes FLASH (BAR1 + 0xD4) with SWRWEn (bit 17) set, which would enable
//! software access to the boot ROM flash (datasheet 6.30). On the deny list.
//! Expected: denied.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::mmio;

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        match mmio::write32(0xD4, 1 << 17) {
            Err(mmio::Error::Denied) => Ok(()),
            other => Err(format!(
                "write32(FLASH 0xd4): expected Err(Denied), got {other:?}"
            )),
        }
    }
}

export!(Module);
