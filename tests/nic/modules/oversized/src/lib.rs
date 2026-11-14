// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control for the robustness contract's frame sizes: like
//! `silent`, but its first `receive` hands up a 1515-byte frame, one over
//! the limit. grind-host must fail the run with `frame-invalid`.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;

thread_local! {
    static SENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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
        if SENT.with(|s| s.replace(true)) {
            return Ok(None);
        }
        Ok(Some(vec![0xff; 1515]))
    }
}

export!(Silent);
