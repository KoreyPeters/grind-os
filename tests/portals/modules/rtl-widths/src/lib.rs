// SPDX-License-Identifier: AGPL-3.0-or-later

//! Writes the RTL8139's 16-bit Interrupt Mask Register (IMR, BAR1 +
//! 0x3C) with write16 and reads it back with read16, then clears it. IMR is
//! not on the deny list.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::{log, mmio};

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        mmio::write16(0x3C, 0x0005).map_err(|e| format!("write16(IMR): {e:?}"))?;
        let got = mmio::read16(0x3C).map_err(|e| format!("read16(IMR): {e:?}"))?;
        mmio::write16(0x3C, 0).map_err(|e| format!("write16(IMR, 0): {e:?}"))?;
        if got != 0x0005 {
            return Err(format!(
                "IMR: expected 0x0005 after write16, got 0x{got:04x}"
            ));
        }
        log::emit(
            log::Level::Info,
            "IMR",
            &[("value".to_string(), format!("0x{got:04x}"))],
        );
        Ok(())
    }
}

export!(Module);
