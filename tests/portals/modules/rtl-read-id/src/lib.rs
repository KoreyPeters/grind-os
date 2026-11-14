// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reads the RTL8139's station address from IDR0-5 (BAR1 + 0x00-0x05,
//! RTL8139C(L)+ datasheet section 6 register table) one byte at a time
//! with read8, and logs it. Shows the window is BAR1, the memory-mapped
//! registers, and that 8-bit reads work.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::{log, mmio};

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = mmio::read8(i as u32).map_err(|e| format!("read8(IDR{i}): {e:?}"))?;
        }
        let text: Vec<String> = mac.iter().map(|b| format!("{b:02x}")).collect();
        log::emit(
            log::Level::Info,
            "IDR",
            &[
                ("mac".to_string(), text.join(":")),
                ("window".to_string(), mmio::size().to_string()),
            ],
        );
        Ok(())
    }
}

export!(Module);
