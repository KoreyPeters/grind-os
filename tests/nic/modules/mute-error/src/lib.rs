// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control for the robustness contract's readable errors: like
//! `silent`, but `receive` returns an empty error. grind-host must fail
//! the run with `driver-error-unreadable` (rule 3), whatever the peer sees.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;

struct Silent;

impl Guest for Silent {
    fn start() -> Result<(), String> {
        Ok(())
    }
}

impl ethernet::Guest for Silent {
    fn mac_address() -> (u8, u8, u8, u8, u8, u8) {
        (0x52, 0x54, 0x00, 0x12, 0x34, 0x56)
    }
    fn link_up() -> bool {
        true
    }
    fn transmit(_frame: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
    fn receive() -> Result<Option<Vec<u8>>, String> {
        Err(String::new())
    }
}

export!(Silent);
