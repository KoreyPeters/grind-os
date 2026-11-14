// SPDX-License-Identifier: AGPL-3.0-or-later

// Shared by the cheat, lenient and partial negative controls. Each includes
// this file at its crate root after defining LENIENT_READ and
// PARTIAL_WRITE. None of them touches the device: reads synthesize the seed
// pattern and writes go to memory.
//
// With both switches off (cheat) it follows the block error contract
// exactly, so every check grind-host makes in the VM passes and only the
// image check can catch it.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "block-driver" });

use exports::grind::portals::block;
use std::cell::RefCell;
use std::collections::BTreeMap;

const BLOCK: usize = 512;
const BLOCKS: u64 = 32768;

thread_local! {
    static WRITTEN: RefCell<BTreeMap<u64, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
}

fn seed(lba: u64) -> Vec<u8> {
    (0..(BLOCK / 16) as u32)
        .flat_map(|r| {
            let mut rec = b"SEED".to_vec();
            rec.extend_from_slice(&lba.to_le_bytes());
            rec.extend_from_slice(&r.to_le_bytes());
            rec
        })
        .collect()
}

fn fits(lba: u64, count: u64) -> bool {
    count > 0 && lba.checked_add(count).is_some_and(|end| end <= BLOCKS)
}

struct Cheat;

impl Guest for Cheat {
    fn start() -> Result<(), String> {
        Ok(())
    }
}

impl block::Guest for Cheat {
    fn block_size() -> u32 {
        BLOCK as u32
    }

    fn capacity() -> u64 {
        BLOCKS
    }

    fn read(lba: u64, count: u32) -> Result<Vec<u8>, String> {
        if !LENIENT_READ && !fits(lba, count.into()) {
            return Err(format!("read: {count} blocks from LBA {lba} do not fit in {BLOCKS}"));
        }
        Ok((0..u64::from(count))
            .map(|i| lba.wrapping_add(i))
            .flat_map(|b| WRITTEN.with(|w| w.borrow().get(&b).cloned()).unwrap_or_else(|| seed(b)))
            .collect())
    }

    fn write(lba: u64, data: Vec<u8>) -> Result<(), String> {
        let whole = !data.is_empty() && data.len() % BLOCK == 0;
        let ok = whole && fits(lba, (data.len() / BLOCK) as u64);
        if !ok && !PARTIAL_WRITE {
            return Err(format!("write: {} bytes at LBA {lba} refused", data.len()));
        }
        // PARTIAL_WRITE: keep the whole blocks that fit, then report failure.
        WRITTEN.with(|w| {
            for (i, chunk) in data.chunks_exact(BLOCK).enumerate() {
                let b = lba + i as u64;
                if b < BLOCKS {
                    w.borrow_mut().insert(b, chunk.to_vec());
                }
            }
        });
        if ok {
            Ok(())
        } else {
            Err(format!("write: {} bytes at LBA {lba} refused (after writing what fit)", data.len()))
        }
    }

    fn flush() -> Result<(), String> {
        Ok(())
    }
}

export!(Cheat);
