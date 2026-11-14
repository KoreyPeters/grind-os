// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control: a "block driver" that never touches the device. It
//! reports the right geometry, reads zeros and accepts writes without doing
//! anything. The block test must fail on it at the first seed read.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "block-driver" });

use exports::grind::portals::block;

struct Fake;

impl Guest for Fake {
    fn start() -> Result<(), String> {
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
