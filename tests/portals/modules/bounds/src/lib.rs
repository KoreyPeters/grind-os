// SPDX-License-Identifier: AGPL-3.0-or-later

//! Out-of-range and misaligned register accesses return the right errors
//! instead of reaching the device. Expected: start returns ok.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::mmio::{self, Error};

struct Module;

fn expect<T: std::fmt::Debug>(
    what: &str,
    got: Result<T, Error>,
    want: Error,
) -> Result<(), String> {
    match got {
        Err(e) if e == want => Ok(()),
        other => Err(format!("{what}: expected Err({want:?}), got {other:?}")),
    }
}

impl Guest for Module {
    fn start() -> Result<(), String> {
        let size = mmio::size();
        if size < 8 || !size.is_multiple_of(4) {
            return Err(format!(
                "window size 0x{size:x}: expected a multiple of 4, at least 8"
            ));
        }
        mmio::read32(size - 4)
            .map_err(|e| format!("read32(last register 0x{:x}): {e:?}", size - 4))?;
        expect("read32(size)", mmio::read32(size), Error::OutOfRange)?;
        expect(
            "read32(0xfffffffc)",
            mmio::read32(0xffff_fffc),
            Error::OutOfRange,
        )?;
        expect("write32(size)", mmio::write32(size, 0), Error::OutOfRange)?;
        expect("read32(0x2)", mmio::read32(0x2), Error::Misaligned)?;
        expect("write32(0x1)", mmio::write32(0x1, 0), Error::Misaligned)?;
        Ok(())
    }
}

export!(Module);
