// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tries to write a word to the 82574's NVM through EEWR (BAR0 + 0x102C,
//! datasheet section 10.2.2.24), which is on the deny list (rule 10).
//!
//! Expected: the write returns `denied` and never reaches the device. This
//! module then returns ok, as a driver that "handles" the error would; the
//! host must still fail the run with `deny-list-violation`.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::mmio::{self, Error};

/// EEPROM Write Register.
const EEWR: u32 = 0x0_102C;
/// START (bit 0) set, word address 0, data 0: "write 0x0000 to NVM word 0".
const WRITE_WORD_0: u32 = 0x0000_0001;

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        match mmio::write32(EEWR, WRITE_WORD_0) {
            Err(Error::Denied) => Ok(()),
            other => Err(format!(
                "write32(EEWR 0x{EEWR:05x}): expected Err(Denied), got {other:?}"
            )),
        }
    }
}

export!(Module);
