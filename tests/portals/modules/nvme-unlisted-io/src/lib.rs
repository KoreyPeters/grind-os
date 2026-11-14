// SPDX-License-Identifier: AGPL-3.0-or-later

//! Submits Write Zeroes (08h), an I/O command not on the list. Expected:
//! refused.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let mut io = Io::new()?;
        let r = io
            .run(command(0x08, 1, 0, 0, [0, 0, 7, 0, 0, 0]))?
            .map(|_| ());
        expect_refused("Write Zeroes", r)
    }
}

export!(Module);
