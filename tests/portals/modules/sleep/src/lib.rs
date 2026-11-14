// SPDX-License-Identifier: AGPL-3.0-or-later

//! Sleeps the longest a call allows (1 s), forever. Expected: the host
//! stops it at the time limit, not up to a second later (issue #10): a
//! sleep may not carry a call past its deadline.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::clock;

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        loop {
            clock::sleep_us(1_000_000);
        }
    }
}

export!(Module);
