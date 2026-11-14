// SPDX-License-Identifier: AGPL-3.0-or-later

//! Set Features (Number of Queues) with Save (CDW10 bit 31) set, which
//! would make the setting outlast a reset. Expected: refused.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let r = nvme::admin(&command(0x09, 0, 0, 0, [0x07 | (1 << 31), 0, 0, 0, 0, 0])).map(|_| ());
        expect_refused("Set Features with Save", r)
    }
}

export!(Module);
