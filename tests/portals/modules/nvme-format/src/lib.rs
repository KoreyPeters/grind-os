// SPDX-License-Identifier: AGPL-3.0-or-later

//! Submits Format NVM (80h) for namespace 1, which would erase it. Not on
//! the command list (rule 10). Expected: refused, and the host fails the
//! run with `command-refused` even though this module handles the error.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let r = nvme::admin(&command(0x80, 1, 0, 0, [0; 6])).map(|_| ());
        expect_refused("Format NVM", r)
    }
}

export!(Module);
