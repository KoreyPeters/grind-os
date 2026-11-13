// SPDX-License-Identifier: AGPL-3.0-or-later

//! DMA buffers: page-aligned, non-overlapping IOVAs, zeroed, data round-trips,
//! bounds enforced, bad sizes refused. Expected: start returns ok.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::dma::{self, Error};

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let a = dma::allocate(4096).map_err(|e| format!("allocate(4096): {e:?}"))?;
        let b = dma::allocate(5000).map_err(|e| format!("allocate(5000): {e:?}"))?;

        if a.size() != 4096 || b.size() != 8192 {
            return Err(format!(
                "sizes: expected 4096 and 8192, got {} and {}",
                a.size(),
                b.size()
            ));
        }
        for (name, buf) in [("a", &a), ("b", &b)] {
            if buf.iova() % 4096 != 0 {
                return Err(format!(
                    "{name}.iova 0x{:x}: expected 4096-aligned",
                    buf.iova()
                ));
            }
        }
        let (a_end, b_end) = (a.iova() + 4096, b.iova() + 8192);
        if a.iova() < b_end && b.iova() < a_end {
            return Err(format!(
                "iovas overlap: a 0x{:x}..0x{a_end:x}, b 0x{:x}..0x{b_end:x}",
                a.iova(),
                b.iova()
            ));
        }

        let zeros = b
            .read(0, 8192)
            .map_err(|e| format!("b.read(0, 8192): {e:?}"))?;
        if zeros.iter().any(|&x| x != 0) {
            return Err("new buffer b is not zeroed".into());
        }

        let pattern: Vec<u8> = (0..=255u8).collect();
        a.write(100, &pattern)
            .map_err(|e| format!("a.write(100, 256 bytes): {e:?}"))?;
        let back = a
            .read(100, 256)
            .map_err(|e| format!("a.read(100, 256): {e:?}"))?;
        if back != pattern {
            return Err("a: bytes read back differ from bytes written".into());
        }

        match a.read(4000, 97) {
            Err(Error::OutOfRange) => {}
            other => {
                return Err(format!(
                    "a.read(4000, 97): expected Err(OutOfRange), got {other:?}"
                ))
            }
        }
        match a.write(4096, &[1]) {
            Err(Error::OutOfRange) => {}
            other => {
                return Err(format!(
                    "a.write(4096, 1 byte): expected Err(OutOfRange), got {other:?}"
                ))
            }
        }
        match dma::allocate(0) {
            Err(Error::BadSize) => {}
            other => {
                return Err(format!(
                    "allocate(0): expected Err(BadSize), got {:?}",
                    other.map(|b| b.iova())
                ))
            }
        }
        Ok(())
    }
}

export!(Module);
