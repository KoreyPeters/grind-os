// SPDX-License-Identifier: AGPL-3.0-or-later

//! Submits vendor-specific admin opcode C0h (SPDK nvme_spec.h:1803).
//! Expected: refused.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let r = nvme::admin(&command(0xC0, 0, 0, 0, [0; 6])).map(|_| ());
        expect_refused("vendor-specific admin C0h", r)
    }
}

export!(Module);
