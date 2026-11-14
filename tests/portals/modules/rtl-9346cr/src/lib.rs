// SPDX-License-Identifier: AGPL-3.0-or-later

//! Writes 0x80 to 9346CR (BAR1 + 0x50) with write8: EEM = 10b, "93C46
//! Programming", which would let the driver bit-bang the EEPROM (datasheet
//! 6.10). On the deny list (rule 10). Expected: denied; the host fails the
//! run even though this module handles the error.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::mmio;

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        match mmio::write8(0x50, 0x80) {
            Err(mmio::Error::Denied) => Ok(()),
            other => Err(format!("write8(9346CR 0x50): expected Err(Denied), got {other:?}")),
        }
    }
}

export!(Module);
