// SPDX-License-Identifier: AGPL-3.0-or-later

//! Aims a Read at IOVA 0x1_0000_0000: the shim's own admin submission
//! queue, its first allocation in the device's domain. If the device wrote
//! there, a driver could plant admin commands (rule 10, device addresses).
//! Expected: refused, because the address is outside the driver's buffers.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let mut io = Io::new()?;
        let r = io.run(rw(0x02, 0, 1, 0x1_0000_0000, 0))?.map(|_| ());
        expect_refused("Read into the shim's admin queue", r)
    }
}

export!(Module);
