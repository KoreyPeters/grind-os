// SPDX-License-Identifier: AGPL-3.0-or-later

//! dma.allocate32 (issue #13): two buffers below 4 GiB. Each must lie
//! entirely below 2^32, start at or above 1 MiB, stay clear of the x86 MSI
//! window (0xfee00000-0xfeefffff), and be separated from the next by a guard
//! page; a write/read round trip must work. An ordinary allocate still comes
//! from 4 GiB up. Logs the IOVAs.

wit_bindgen::generate!({ path: "../../../../portals/wit", world: "driver" });

use grind::portals::{dma, log};

const GIB4: u64 = 1 << 32;
const MSI: (u64, u64) = (0xfee0_0000, 0xfef0_0000);

fn check_low(name: &str, b: &dma::Buffer) -> Result<(), String> {
    let (start, end) = (b.iova(), b.iova() + u64::from(b.size()));
    if start < (1 << 20) || end > GIB4 || (start < MSI.1 && MSI.0 < end) {
        return Err(format!(
            "{name}: expected an IOVA range from 1 MiB, below 4 GiB, clear of the MSI window; got 0x{start:x}-0x{end:x}"
        ));
    }
    Ok(())
}

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let a = dma::allocate32(8192).map_err(|e| format!("allocate32(8192): {e:?}"))?;
        let b = dma::allocate32(4096).map_err(|e| format!("allocate32(4096): {e:?}"))?;
        let high = dma::allocate(4096).map_err(|e| format!("allocate(4096): {e:?}"))?;
        check_low("first", &a)?;
        check_low("second", &b)?;
        if b.iova() < a.iova() + u64::from(a.size()) + 4096 {
            return Err(format!(
                "expected a guard page between buffers; got 0x{:x}+0x{:x} then 0x{:x}",
                a.iova(),
                a.size(),
                b.iova()
            ));
        }
        if high.iova() < GIB4 {
            return Err(format!(
                "allocate: expected an IOVA from 4 GiB up, got 0x{:x}",
                high.iova()
            ));
        }
        let pattern: Vec<u8> = (0..8192u32).map(|i| (i * 13 + 7) as u8).collect();
        a.write(0, &pattern).map_err(|e| format!("write: {e:?}"))?;
        if a.read(0, 8192).map_err(|e| format!("read: {e:?}"))? != pattern {
            return Err("round trip through a low buffer differs".into());
        }
        log::emit(
            log::Level::Info,
            "dma32",
            &[
                ("first".to_string(), format!("0x{:x}", a.iova())),
                ("second".to_string(), format!("0x{:x}", b.iova())),
                ("high".to_string(), format!("0x{:x}", high.iova())),
            ],
        );
        Ok(())
    }
}

export!(Module);
