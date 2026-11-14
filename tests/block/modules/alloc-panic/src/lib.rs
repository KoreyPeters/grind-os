// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control for the robustness contract's allocation failure:
//! `start` allocates two DMA buffers and unwraps each, so when the second
//! allocation fails it panics (a trap) instead of returning a readable
//! error. tests/block's alloc-fail step must fail.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "block-driver" });

use exports::grind::portals::block;

thread_local! {
    static BUFS: std::cell::RefCell<Vec<grind::portals::dma::Buffer>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct Fake;

impl Guest for Fake {
    fn start() -> Result<(), String> {
        let a = grind::portals::dma::allocate(4096).unwrap();
        let b = grind::portals::dma::allocate(4096).unwrap();
        BUFS.with(|v| v.borrow_mut().extend([a, b]));
        Ok(())
    }
}

impl block::Guest for Fake {
    fn block_size() -> u32 {
        512
    }
    fn capacity() -> u64 {
        32768
    }
    fn read(_lba: u64, count: u32) -> Result<Vec<u8>, String> {
        Ok(vec![0; count as usize * 512])
    }
    fn write(_lba: u64, _data: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
    fn flush() -> Result<(), String> {
        Ok(())
    }
}

export!(Fake);
