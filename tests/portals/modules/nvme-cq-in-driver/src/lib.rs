// SPDX-License-Identifier: AGPL-3.0-or-later

//! Creates an I/O completion queue in the driver's own buffer. The shim
//! allocates every queue, so PRP1 must be 0. Expected: refused.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let buf = dma::allocate(4096).map_err(|e| format!("allocate: {e:?}"))?;
        let r = nvme::admin(&command(0x05, 0, buf.iova(), 0, [(15 << 16) | 1, 1, 0, 0, 0, 0])).map(|_| ());
        expect_refused("Create I/O CQ in driver memory", r)
    }
}

export!(Module);
