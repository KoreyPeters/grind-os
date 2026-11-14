// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control for the robustness contract's "no growth in DMA
//! memory after start": like `silent`, but every `receive` allocates a DMA
//! buffer and keeps it. grind-host must fail the run with `dma-growth`.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "nic" });

use exports::grind::portals::ethernet;

thread_local! {
    static LEAKED: std::cell::RefCell<Vec<grind::portals::dma::Buffer>> = const { std::cell::RefCell::new(Vec::new()) };
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
        if let Ok(b) = grind::portals::dma::allocate(4096) {
            LEAKED.with(|l| l.borrow_mut().push(b));
        }
        Ok(None)
    }
}

export!(Silent);
