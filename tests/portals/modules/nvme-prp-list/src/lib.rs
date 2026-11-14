// SPDX-License-Identifier: AGPL-3.0-or-later

//! A 12 KiB Read (24 blocks), which needs a PRP list in driver memory the
//! driver could change after the check. Expected: refused.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let mut io = Io::new()?;
        let base = io.data.iova();
        let r = io.run(rw(0x02, 0, 24, base, base + 0x1000))?.map(|_| ());
        expect_refused("Read needing a PRP list", r)
    }
}

export!(Module);
