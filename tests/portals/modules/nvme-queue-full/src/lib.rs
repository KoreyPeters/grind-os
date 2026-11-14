// SPDX-License-Identifier: AGPL-3.0-or-later

//! Flow control: submits Flushes to the 16-entry SQ 1 without collecting
//! completions. The shim knows only from completions it has read itself
//! how far the device has fetched, so the 16th submit must get
//! `queue-full`. After collecting the 15 completions, submitting works
//! again.

include!("../../nvme-common/common.rs");

struct Module;

impl Guest for Module {
    fn start() -> Result<(), String> {
        let mut io = Io::new()?;
        let flush = || command(0x00, 1, 0, 0, [0; 6]);
        for i in 0..15 {
            io.submit(flush()).map_err(|e| format!("submit {i}: {e:?}"))?;
        }
        match io.submit(flush()) {
            Err(nvme::Error::QueueFull) => {}
            other => return Err(format!("16th submit: expected Err(QueueFull), got {other:?}")),
        }
        let done = io.collect(15)?;
        if let Some(bad) = done.iter().find(|c| status(c) != 0) {
            return Err(format!("a flush completed with status 0x{:04x}", status(bad)));
        }
        let again = io.run(flush())?;
        if again != Ok(0) {
            return Err(format!("submit after collecting: expected Ok(0), got {again:?}"));
        }
        info("nvme-queue-full", &[("queue_full_after", "15".into()), ("then", "ok".into())]);
        Ok(())
    }
}

export!(Module);
