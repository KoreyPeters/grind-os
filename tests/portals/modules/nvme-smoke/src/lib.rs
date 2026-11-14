// SPDX-License-Identifier: AGPL-3.0-or-later

//! The positive NVMe portal case: Identify the controller into the
//! driver's own buffer and check its serial number, create an I/O queue
//! pair, write a pattern to blocks 8-15, read it back exactly, and flush.
//! Every command goes through the shim's checks; none may be refused.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let id = dma::allocate(4096).map_err(|e| format!("allocate: {e:?}"))?;
        admin_ok(
            "Identify Controller",
            command(0x06, 0, id.iova(), 0, [1, 0, 0, 0, 0, 0]),
        )?;
        // Serial number: bytes 4-23 (FreeBSD nvme.h:1313-1319).
        let sn = id.read(4, 20).map_err(|e| format!("read: {e:?}"))?;
        let sn = String::from_utf8_lossy(&sn).trim().to_string();
        if sn != "grind0001" {
            return Err(format!(
                "serial number: expected grind0001 (vm/run.sh --nvme), got {sn:?}"
            ));
        }
        let mut io = Io::new()?;
        let pattern: Vec<u8> = (0..4096u32).map(|i| (i * 7 + 3) as u8).collect();
        io.data
            .write(0, &pattern)
            .map_err(|e| format!("write data: {e:?}"))?;
        let base = io.data.iova();
        let w = io.run(rw(0x01, 8, 8, base, 0))?;
        let r = io.run(rw(0x02, 8, 8, base + 0x2000, 0))?;
        let f = io.run(command(0x00, 1, 0, 0, [0; 6]))?;
        if w != Ok(0) || r != Ok(0) || f != Ok(0) {
            return Err(format!(
                "write {w:?}, read {r:?}, flush {f:?}: expected Ok(0) each"
            ));
        }
        let back = io
            .data
            .read(0x2000, 4096)
            .map_err(|e| format!("read data: {e:?}"))?;
        if back != pattern {
            let at = back
                .iter()
                .zip(&pattern)
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            return Err(format!(
                "read back differs at byte {at}: 0x{:02x}, wrote 0x{:02x}",
                back[at], pattern[at]
            ));
        }
        info(
            "nvme-smoke",
            &[("serial", sn), ("round_trip_bytes", "4096".into())],
        );
        Ok(())
    }
}

export!(Module);
