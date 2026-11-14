// SPDX-License-Identifier: AGPL-3.0-or-later

//! The NVMe controller does DMA to and from a buffer from dma.allocate32
//! (issue #13): write 16 blocks (two pages, both PRP entries) at LBA 64
//! from a low buffer, read them back into the same buffer's other half, and
//! flush. Shows a real device reaches IOVAs below 4 GiB, and that the
//! shim's address checks accept the driver's low buffers.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let data = dma::allocate32(16384).map_err(|e| format!("allocate32: {e:?}"))?;
        let iova = data.iova();
        if iova + 16384 > 1 << 32 {
            return Err(format!("allocate32: expected an IOVA below 4 GiB, got 0x{iova:x}"));
        }
        let mut io = Io::with_data(data)?;
        let pattern: Vec<u8> = (0..8192u32).map(|i| (i * 11 + 5) as u8).collect();
        io.data.write(0, &pattern).map_err(|e| format!("write data: {e:?}"))?;
        let w = io.run(rw(0x01, 64, 16, iova, iova + 0x1000))?;
        let r = io.run(rw(0x02, 64, 16, iova + 0x2000, iova + 0x3000))?;
        let f = io.run(command(0x00, 1, 0, 0, [0; 6]))?;
        if w != Ok(0) || r != Ok(0) || f != Ok(0) {
            return Err(format!("write {w:?}, read {r:?}, flush {f:?}: expected Ok(0) each"));
        }
        let back = io.data.read(0x2000, 8192).map_err(|e| format!("read data: {e:?}"))?;
        if back != pattern {
            let at = back.iter().zip(&pattern).position(|(a, b)| a != b).unwrap_or(0);
            return Err(format!("read back differs at byte {at}"));
        }
        info("nvme-dma32", &[("iova", format!("0x{iova:x}")), ("round_trip_bytes", "8192".into())]);
        Ok(())
    }
}

export!(Module);
