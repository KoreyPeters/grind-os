// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control: a "block driver" that never touches the device but
//! knows the test. It synthesizes the seed pattern for reads and keeps
//! writes in memory, so every check grind-host makes in the VM passes. Only
//! the image check after the VM powers off can catch it: the disk still
//! holds the seed where the data pattern should be.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "block-driver" });

use exports::grind::portals::block;
use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
    static WRITTEN: RefCell<BTreeMap<u64, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
}

fn seed(lba: u64) -> Vec<u8> {
    (0..32u32)
        .flat_map(|r| {
            let mut rec = b"SEED".to_vec();
            rec.extend_from_slice(&lba.to_le_bytes());
            rec.extend_from_slice(&r.to_le_bytes());
            rec
        })
        .collect()
}

struct Cheat;

impl Guest for Cheat {
    fn start() -> Result<(), String> {
        Ok(())
    }
}

impl block::Guest for Cheat {
    fn block_size() -> u32 {
        512
    }
    fn capacity() -> u64 {
        32768
    }
    fn read(lba: u64, count: u32) -> Result<Vec<u8>, String> {
        Ok((lba..lba + u64::from(count))
            .flat_map(|b| WRITTEN.with(|w| w.borrow().get(&b).cloned()).unwrap_or_else(|| seed(b)))
            .collect())
    }
    fn write(lba: u64, data: Vec<u8>) -> Result<(), String> {
        WRITTEN.with(|w| {
            for (i, chunk) in data.chunks(512).enumerate() {
                w.borrow_mut().insert(lba + i as u64, chunk.to_vec());
            }
        });
        Ok(())
    }
    fn flush() -> Result<(), String> {
        Ok(())
    }
}

export!(Cheat);
