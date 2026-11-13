// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reads the 82574's Device Status register (BAR0 + 0x00008) through the
//! mmio portal and logs it. Expected: start returns ok.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::{log, mmio};

const STATUS: u32 = 0x0_0008;

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let value =
            mmio::read32(STATUS).map_err(|e| format!("read32(0x{STATUS:05x}) failed: {e:?}"))?;
        if value == u32::MAX {
            return Err("STATUS read 0xffffffff: the device did not respond".into());
        }
        log::emit(
            log::Level::Info,
            "STATUS",
            &[("value".into(), format!("0x{value:08x}"))],
        );
        Ok(())
    }
}

export!(Module);
