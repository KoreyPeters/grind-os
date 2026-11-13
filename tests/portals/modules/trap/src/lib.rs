// SPDX-License-Identifier: AGPL-3.0-or-later

//! Panics (a Wasm trap). Expected: the host reports the trap with a
//! backtrace naming this function.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

struct Module;

#[inline(never)]
fn deliberate_trap(n: u32) -> u32 {
    if std::hint::black_box(n) < 10 {
        panic!("deliberate trap");
    }
    n
}

impl Guest for Module {
    fn start() -> Result<(), String> {
        deliberate_trap(1);
        Ok(())
    }
}

export!(Module);
