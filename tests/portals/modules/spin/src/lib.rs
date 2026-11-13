// SPDX-License-Identifier: AGPL-3.0-or-later

//! Never returns. Expected: the host stops it at the time limit and reports
//! a timeout.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let mut n: u64 = 0;
        loop {
            n = std::hint::black_box(n.wrapping_add(1));
        }
    }
}

export!(Module);
