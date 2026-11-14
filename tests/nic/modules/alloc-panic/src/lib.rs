// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control for the robustness contract's allocation failure:
//! `start` allocates two DMA buffers and unwraps each, so when the second
//! allocation fails it panics (a trap) instead of returning a readable
//! error. tests/nic's alloc-fail step must fail.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;

thread_local! {
    static BUFS: std::cell::RefCell<Vec<grind::portals::dma::Buffer>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct Silent;

impl Guest for Silent {
    fn start() -> Result<(), String> {
        let a = grind::portals::dma::allocate(4096).unwrap();
        let b = grind::portals::dma::allocate(4096).unwrap();
        BUFS.with(|v| v.borrow_mut().extend([a, b]));
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
        Ok(None)
    }
}

export!(Silent);
