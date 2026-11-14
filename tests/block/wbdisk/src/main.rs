// SPDX-License-Identifier: AGPL-3.0-or-later

//! wbdisk: a write-back disk for the block conformance test.
//!
//! ```text
//! wbdisk IMAGE SOCKET
//! ```
//!
//! Serves the raw disk image IMAGE to QEMU over NBD on the Unix socket
//! SOCKET. Writes are kept in memory, as in a disk's volatile write cache,
//! and reach the image file only when QEMU sends a flush (which it does
//! when the guest's driver flushes). Killing wbdisk loses exactly the
//! writes since the last flush, as a power cut would, so the block test can
//! check that a driver's `flush` really makes its writes durable
//! (docs/design/robust-block.md).
//!
//! NBD, as far as QEMU needs it: the fixed newstyle handshake, the
//! `EXPORT_NAME` and `GO` options (others are refused as unsupported), and
//! the `READ`, `WRITE`, `FLUSH` and `DISC` commands with simple replies. No
//! FUA, no structured replies. Written from the NBD protocol description
//! (NetworkBlockDevice/nbd, `doc/proto.md`); facts only. Std only.
//!
//! Logs one line per handshake step and per flush to stderr.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixListener;

const NBDMAGIC: u64 = 0x4e42_444d_4147_4943;
const IHAVEOPT: u64 = 0x4948_4156_454f_5054;
const OPT_REPLY_MAGIC: u64 = 0x0003_e889_0455_65a9;
const REQUEST_MAGIC: u32 = 0x2560_9513;
const SIMPLE_REPLY_MAGIC: u32 = 0x6744_6698;

const FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
const FLAG_NO_ZEROES: u16 = 1 << 1;
const CLIENT_NO_ZEROES: u32 = 1 << 1;

const OPT_EXPORT_NAME: u32 = 1;
const OPT_ABORT: u32 = 2;
const OPT_GO: u32 = 7;
const REP_ACK: u32 = 1;
const REP_INFO: u32 = 3;
const REP_ERR_UNSUP: u32 = (1 << 31) + 1;
const INFO_EXPORT: u16 = 0;

/// Transmission flags: has flags, can flush. No FUA: writes are durable
/// only through a flush.
const TRANSMIT_FLAGS: u16 = (1 << 0) | (1 << 2);

const CMD_READ: u16 = 0;
const CMD_WRITE: u16 = 1;
const CMD_DISC: u16 = 2;
const CMD_FLUSH: u16 = 3;
const EIO: u32 = 5;
const EINVAL: u32 = 22;

/// The overlay's unit.
const BLOCK: u64 = 512;

/// The image plus the writes not yet flushed to it.
struct Disk {
    file: File,
    size: u64,
    /// Unflushed blocks: block number -> contents.
    dirty: BTreeMap<u64, Vec<u8>>,
}

impl Disk {
    fn read(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        self.file.read_exact_at(&mut out, offset)?;
        // Overlay the unflushed blocks this range touches.
        let first = offset / BLOCK;
        let last = (offset + len as u64).div_ceil(BLOCK);
        for (&b, data) in self.dirty.range(first..last) {
            let start = b * BLOCK;
            let lo = start.max(offset);
            let hi = (start + BLOCK).min(offset + len as u64);
            out[(lo - offset) as usize..(hi - offset) as usize]
                .copy_from_slice(&data[(lo - start) as usize..(hi - start) as usize]);
        }
        Ok(out)
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let end = offset + data.len() as u64;
        let mut b = offset / BLOCK;
        while b * BLOCK < end {
            let start = b * BLOCK;
            let mut block = match self.dirty.get(&b) {
                Some(d) => d.clone(),
                None => self.read(start, BLOCK as usize)?,
            };
            let lo = start.max(offset);
            let hi = (start + BLOCK).min(end);
            block[(lo - start) as usize..(hi - start) as usize]
                .copy_from_slice(&data[(lo - offset) as usize..(hi - offset) as usize]);
            self.dirty.insert(b, block);
            b += 1;
        }
        Ok(())
    }

    /// Write every unflushed block to the file and sync it.
    fn flush(&mut self) -> io::Result<usize> {
        let n = self.dirty.len();
        for (&b, data) in &self.dirty {
            self.file.write_all_at(data, b * BLOCK)?;
        }
        self.file.sync_all()?;
        self.dirty.clear();
        Ok(n)
    }
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}
fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}
fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn option_reply(w: &mut impl Write, option: u32, kind: u32, data: &[u8]) -> io::Result<()> {
    w.write_all(&OPT_REPLY_MAGIC.to_be_bytes())?;
    w.write_all(&option.to_be_bytes())?;
    w.write_all(&kind.to_be_bytes())?;
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(data)
}

/// The handshake. Ok(true) when transmission should start.
fn handshake(s: &mut (impl Read + Write), size: u64) -> io::Result<bool> {
    s.write_all(&NBDMAGIC.to_be_bytes())?;
    s.write_all(&IHAVEOPT.to_be_bytes())?;
    s.write_all(&(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes())?;
    let client = read_u32(s)?;
    eprintln!("wbdisk: client flags 0x{client:x}");
    loop {
        if read_u64(s)? != IHAVEOPT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "option without IHAVEOPT"));
        }
        let option = read_u32(s)?;
        let len = read_u32(s)? as usize;
        let mut data = vec![0u8; len];
        s.read_exact(&mut data)?;
        eprintln!("wbdisk: option {option}, {len} bytes");
        match option {
            OPT_EXPORT_NAME => {
                s.write_all(&size.to_be_bytes())?;
                s.write_all(&TRANSMIT_FLAGS.to_be_bytes())?;
                if client & CLIENT_NO_ZEROES == 0 {
                    s.write_all(&[0u8; 124])?;
                }
                return Ok(true);
            }
            OPT_GO => {
                let mut info = INFO_EXPORT.to_be_bytes().to_vec();
                info.extend_from_slice(&size.to_be_bytes());
                info.extend_from_slice(&TRANSMIT_FLAGS.to_be_bytes());
                option_reply(s, option, REP_INFO, &info)?;
                option_reply(s, option, REP_ACK, &[])?;
                return Ok(true);
            }
            OPT_ABORT => {
                option_reply(s, option, REP_ACK, &[])?;
                return Ok(false);
            }
            _ => option_reply(s, option, REP_ERR_UNSUP, &[])?,
        }
    }
}

fn simple_reply(w: &mut impl Write, error: u32, handle: u64, data: &[u8]) -> io::Result<()> {
    w.write_all(&SIMPLE_REPLY_MAGIC.to_be_bytes())?;
    w.write_all(&error.to_be_bytes())?;
    w.write_all(&handle.to_be_bytes())?;
    w.write_all(data)
}

/// Serve commands until the client disconnects.
fn transmission(s: &mut (impl Read + Write), disk: &mut Disk) -> io::Result<()> {
    loop {
        let magic = match read_u32(s) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        if magic != REQUEST_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("request magic 0x{magic:x}")));
        }
        let _flags = read_u16(s)?;
        let kind = read_u16(s)?;
        let handle = read_u64(s)?;
        let offset = read_u64(s)?;
        let len = read_u32(s)? as usize;
        let in_range = offset.checked_add(len as u64).is_some_and(|end| end <= disk.size);
        match kind {
            CMD_READ if in_range => match disk.read(offset, len) {
                Ok(d) => simple_reply(s, 0, handle, &d)?,
                Err(_) => simple_reply(s, EIO, handle, &[])?,
            },
            CMD_WRITE => {
                let mut data = vec![0u8; len];
                s.read_exact(&mut data)?;
                let err = if !in_range {
                    EINVAL
                } else if disk.write(offset, &data).is_err() {
                    EIO
                } else {
                    0
                };
                simple_reply(s, err, handle, &[])?;
            }
            CMD_FLUSH => match disk.flush() {
                Ok(n) => {
                    eprintln!("wbdisk: flush wrote {n} blocks");
                    simple_reply(s, 0, handle, &[])?;
                }
                Err(e) => {
                    eprintln!("wbdisk: flush failed: {e}");
                    simple_reply(s, EIO, handle, &[])?;
                }
            },
            CMD_DISC => return Ok(()),
            _ => simple_reply(s, EINVAL, handle, &[])?,
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: wbdisk IMAGE SOCKET");
        std::process::exit(2);
    }
    let file = OpenOptions::new().read(true).write(true).open(&args[1]).unwrap_or_else(|e| {
        eprintln!("wbdisk: open {}: {e}", args[1]);
        std::process::exit(1)
    });
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut disk = Disk { file, size, dirty: BTreeMap::new() };
    let _ = std::fs::remove_file(&args[2]);
    let listener = UnixListener::bind(&args[2]).unwrap_or_else(|e| {
        eprintln!("wbdisk: bind {}: {e}", args[2]);
        std::process::exit(1)
    });
    eprintln!("wbdisk: serving {} ({size} bytes) on {}", args[1], args[2]);
    // QEMU may connect more than once (it probes, then opens).
    for conn in listener.incoming() {
        let mut s = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wbdisk: accept: {e}");
                continue;
            }
        };
        match handshake(&mut s, size) {
            Ok(true) => {
                if let Err(e) = transmission(&mut s, &mut disk) {
                    eprintln!("wbdisk: connection ended: {e}");
                }
                eprintln!("wbdisk: client gone; {} unflushed blocks kept in memory", disk.dirty.len());
            }
            Ok(false) => eprintln!("wbdisk: client aborted the handshake"),
            Err(e) => eprintln!("wbdisk: handshake failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(blocks: u64) -> (Disk, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("wbdisk-test-{}-{blocks}", std::process::id()));
        std::fs::write(&path, vec![0x11u8; (blocks * BLOCK) as usize]).unwrap();
        let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        (Disk { file, size: blocks * BLOCK, dirty: BTreeMap::new() }, path)
    }

    #[test]
    fn writes_stay_in_memory_until_a_flush() {
        let (mut d, path) = disk(8);
        d.write(BLOCK, &[0x22; 512]).unwrap();
        assert_eq!(d.read(BLOCK, 512).unwrap(), vec![0x22; 512], "reads see the write");
        let on_file = std::fs::read(&path).unwrap();
        assert_eq!(on_file[512..1024], [0x11; 512], "not on the file yet");
        assert_eq!(d.flush().unwrap(), 1);
        let on_file = std::fs::read(&path).unwrap();
        assert_eq!(on_file[512..1024], [0x22; 512], "on the file after flush");
        assert!(d.dirty.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unaligned_writes_and_reads_across_blocks() {
        let (mut d, path) = disk(4);
        d.write(500, &[0x33; 30]).unwrap(); // bytes 500..530: blocks 0 and 1
        assert_eq!(d.dirty.len(), 2);
        let r = d.read(490, 50).unwrap();
        assert_eq!(&r[..10], &[0x11; 10]);
        assert_eq!(&r[10..40], &[0x33; 30]);
        assert_eq!(&r[40..], &[0x11; 10]);
        // A dropped Disk loses its unflushed blocks, like a power cut.
        drop(d);
        assert_eq!(std::fs::read(&path).unwrap(), vec![0x11; 2048]);
        std::fs::remove_file(path).unwrap();
    }
}
