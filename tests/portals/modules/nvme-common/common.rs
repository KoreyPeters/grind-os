// SPDX-License-Identifier: AGPL-3.0-or-later

// Shared code for the NVMe portal test modules: the world bindings, command
// building, and one I/O queue pair. Each module includes this file at its
// crate root (wit-bindgen 0.63 cannot export a world's export macro from a
// library crate). Trusted test code, not a driver.
//
// Layouts follow FreeBSD's nvme(4) (`struct nvme_command`,
// `struct nvme_completion`, sys/dev/nvme/nvme.h:890-940) at commit
// 74d72b3491e810068313e49629c3b8fe46f29318. The shim owns every queue and
// doorbell; completions come back through the portal.

wit_bindgen::generate!({
    path: "../../../../portals/wit",
    world: "nvme-driver",
});

#[allow(unused_imports)]
use grind::portals::{clock, dma, log, mmio, nvme};

/// Entries in the test I/O queues.
pub const QUEUE_ENTRIES: u16 = 16;

/// A 64-byte command with the given fields; everything else zero.
pub fn command(opcode: u8, nsid: u32, prp1: u64, prp2: u64, cdw: [u32; 6]) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[0] = opcode;
    b[4..8].copy_from_slice(&nsid.to_le_bytes());
    b[24..32].copy_from_slice(&prp1.to_le_bytes());
    b[32..40].copy_from_slice(&prp2.to_le_bytes());
    for (i, d) in cdw.iter().enumerate() {
        b[40 + 4 * i..44 + 4 * i].copy_from_slice(&d.to_le_bytes());
    }
    b
}

/// Completion status (dword 3 bits 31:16) with the phase bit removed.
pub fn status(cpl: &[u8]) -> u16 {
    u16::from_le_bytes([cpl[14], cpl[15]]) >> 1
}

/// Run an admin command and require success.
pub fn admin_ok(what: &str, cmd: Vec<u8>) -> Result<Vec<u8>, String> {
    let cpl = nvme::admin(&cmd).map_err(|e| format!("{what}: portal error {e:?}"))?;
    match status(&cpl) {
        0 => Ok(cpl),
        s => Err(format!("{what}: completion status 0x{s:04x}")),
    }
}

pub fn info(message: &str, fields: &[(&str, String)]) {
    let f: Vec<(String, String)> = fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    log::emit(log::Level::Info, message, &f);
}

/// I/O queue pair 1 (both allocated by the shim) and a 16 KiB data buffer.
pub struct Io {
    pub data: dma::Buffer,
    next_cid: u16,
}

impl Io {
    /// Ask for one queue pair, then create completion queue 1 and
    /// submission queue 1. PRP1 = 0 for both: the shim allocates them.
    pub fn new() -> Result<Io, String> {
        admin_ok("Set Features (Number of Queues)", command(0x09, 0, 0, 0, [0x07, 0, 0, 0, 0, 0]))?;
        let data = dma::allocate(16384).map_err(|e| format!("allocate data: {e:?}"))?;
        let size = u32::from(QUEUE_ENTRIES - 1) << 16;
        admin_ok("Create I/O CQ 1", command(0x05, 0, 0, 0, [size | 1, 1, 0, 0, 0, 0]))?;
        admin_ok("Create I/O SQ 1", command(0x01, 0, 0, 0, [size | 1, (1 << 16) | 1, 0, 0, 0, 0]))?;
        Ok(Io { data, next_cid: 1 })
    }

    /// Submit `cmd` on SQ 1 with a fresh command identifier; return it.
    pub fn submit(&mut self, mut cmd: Vec<u8>) -> Result<u16, nvme::Error> {
        let cid = self.next_cid;
        self.next_cid += 1;
        cmd[2..4].copy_from_slice(&cid.to_le_bytes());
        nvme::submit(1, &cmd).map(|()| cid)
    }

    /// Wait up to a second for `n` completions on CQ 1.
    pub fn collect(&mut self, n: usize) -> Result<Vec<Vec<u8>>, String> {
        let mut got = Vec::new();
        let start = clock::now_ns();
        while got.len() < n {
            let batch = nvme::complete(1).map_err(|e| format!("complete(1): {e:?}"))?;
            if batch.is_empty() {
                if clock::now_ns() - start > 1_000_000_000 {
                    return Err(format!("{} of {n} completions within 1 s", got.len()));
                }
                clock::sleep_us(10);
            }
            got.extend(batch);
        }
        Ok(got)
    }

    /// Submit `cmd` on SQ 1 and wait for its completion. Returns the
    /// portal error, or the completion status.
    pub fn run(&mut self, cmd: Vec<u8>) -> Result<Result<u16, nvme::Error>, String> {
        let cid = match self.submit(cmd) {
            Ok(cid) => cid,
            Err(e) => return Ok(Err(e)),
        };
        let cpl = self.collect(1)?.remove(0);
        if cpl.len() != 16 {
            return Err(format!("completion of {} bytes, expected 16", cpl.len()));
        }
        let got = u16::from_le_bytes([cpl[12], cpl[13]]);
        if got != cid {
            return Err(format!("completion for command {got}, expected {cid}"));
        }
        Ok(Ok(status(&cpl)))
    }
}

/// Read or Write `blocks` 512-byte blocks at `lba` of namespace 1.
pub fn rw(opcode: u8, lba: u64, blocks: u32, prp1: u64, prp2: u64) -> Vec<u8> {
    command(opcode, 1, prp1, prp2, [lba as u32, (lba >> 32) as u32, blocks - 1, 0, 0, 0])
}

/// For a module that submits something the shim must refuse: Ok if the
/// portal said `refused` (the host must still fail the run), else why not.
pub fn expect_refused(what: &str, r: Result<(), nvme::Error>) -> Result<(), String> {
    match r {
        Err(nvme::Error::Refused) => Ok(()),
        other => Err(format!("{what}: expected Err(Refused), got {other:?}")),
    }
}
