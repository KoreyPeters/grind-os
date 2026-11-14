// SPDX-License-Identifier: AGPL-3.0-or-later

//! `--exercise-block`: the in-VM half of the block conformance test
//! (tests/block). After `start`, the host calls the driver's `block`
//! exports and checks every answer against patterns it computes itself.
//!
//! The disk image starts with the seed pattern; the host reads the first
//! block, the last block, a 40-block run and a 256-block run (more than one
//! command can carry, so the driver must split them), writes the data
//! pattern to the same places, flushes, and reads them back.
//!
//! Then the error contract (`portals/wit`, interface `block`): reads and
//! writes that do not fit the disk, empty ones, and a write that is not a
//! whole number of blocks must each return a non-empty error. A write that
//! runs past the end must write nothing, not even the blocks that fit, so
//! afterwards the host reads the last and first blocks again: they must
//! still hold the data pattern, which also shows the driver kept working.
//!
//! tests/block/check.sh then checks the image file itself after the VM
//! powers off, so a driver that only pretends (for example, keeping writes
//! in memory) still fails.
//!
//! Patterns, in 16-byte records: record `r` of block `lba` is the 4-byte
//! tag (`SEED`, `DATA` or `BADW`), `lba` as a little-endian u64, then `r`
//! as a little-endian u32. Every block differs from every other block, so a
//! read from the wrong place cannot match.

use grind_shim::log::{emit, Value};
use std::time::Duration;
use wasmtime::Store;

use crate::block_world::exports::grind::portals::block::Guest as Block;
use crate::{call_failed, driver_error, set_deadline, State, CALL_LIMIT};

pub const SEED: &[u8; 4] = b"SEED";
pub const DATA: &[u8; 4] = b"DATA";
/// Written only by calls that must fail; never expected on the disk.
pub const BADW: &[u8; 4] = b"BADW";
/// First block of the multi-block run.
pub const RUN_LBA: u64 = 1000;
/// Length of the multi-block run, in blocks.
pub const RUN_BLOCKS: u32 = 40;
/// First block of the long run.
pub const LONG_LBA: u64 = 8192;
/// Length of the long run, in blocks (128 KiB of 512-byte blocks).
pub const LONG_BLOCKS: u32 = 256;
/// The block a short write must not touch.
pub const SHORT_WRITE_LBA: u64 = 5;
/// Recovery checks (after every hostile phase) write and read back the
/// RCVR pattern here; the image check expects it at the end.
pub const RCVR: &[u8; 4] = b"RCVR";
pub const RECOVERY_LBA: u64 = 2000;
pub const RECOVERY_BLOCKS: u32 = 8;

/// What the test expects of the disk.
pub struct Expect {
    pub block_size: u32,
    pub blocks: u64,
}

/// `count` blocks of a pattern starting at `lba`.
pub fn pattern(tag: &[u8; 4], lba: u64, count: u32, block_size: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(count as usize * block_size as usize);
    for b in 0..u64::from(count) {
        for r in 0..block_size / 16 {
            out.extend_from_slice(tag);
            out.extend_from_slice(&(lba + b).to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
        }
    }
    out
}

/// Compare what a read returned with what it should have; log and fail on
/// the first difference.
fn compare(
    call: &str,
    lba: u64,
    count: u32,
    block_size: u32,
    want: &[u8],
    got: &[u8],
) -> Result<(), &'static str> {
    if got.len() != want.len() {
        emit(
            "block-mismatch",
            &[
                ("call", Value::Str(call)),
                ("lba", Value::U64(lba)),
                ("count", Value::U64(count.into())),
                ("expected", Value::Str(&format!("{} bytes", want.len()))),
                ("got", Value::Str(&format!("{} bytes", got.len()))),
            ],
        );
        return Err("block-mismatch");
    }
    if let Some(at) = want.iter().zip(got).position(|(a, b)| a != b) {
        let record = at / 16 * 16;
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        emit(
            "block-mismatch",
            &[
                ("call", Value::Str(call)),
                ("lba", Value::U64(lba)),
                ("count", Value::U64(count.into())),
                ("first_bad_byte", Value::U64(at as u64)),
                ("block", Value::U64(lba + (at / block_size as usize) as u64)),
                ("expected", Value::Str(&hex(&want[record..record + 16]))),
                ("got", Value::Str(&hex(&got[record..record + 16]))),
            ],
        );
        return Err("block-mismatch");
    }
    emit(
        "block-ok",
        &[
            ("call", Value::Str(call)),
            ("lba", Value::U64(lba)),
            ("count", Value::U64(count.into())),
            ("bytes", Value::U64(got.len() as u64)),
        ],
    );
    Ok(())
}

/// Run the exercise. Returns the failure reason, if any.
pub fn exercise(
    store: &mut Store<State>,
    blk: &Block,
    expect: &Expect,
) -> Result<(), &'static str> {
    let limit: Duration = CALL_LIMIT;

    set_deadline(store, limit);
    let block_size = blk
        .call_block_size(&mut *store)
        .map_err(|e| call_failed("block-size", limit, e))?;
    set_deadline(store, limit);
    let blocks = blk
        .call_capacity(&mut *store)
        .map_err(|e| call_failed("capacity", limit, e))?;
    let ok = block_size == expect.block_size && blocks == expect.blocks;
    emit(
        if ok {
            "block-geometry"
        } else {
            "block-geometry-mismatch"
        },
        &[
            (
                "expected",
                Value::Str(&format!(
                    "{} blocks of {} bytes",
                    expect.blocks, expect.block_size
                )),
            ),
            (
                "got",
                Value::Str(&format!("{blocks} blocks of {block_size} bytes")),
            ),
        ],
    );
    if !ok {
        return Err("block-geometry");
    }

    let last = blocks - 1;
    let places = [
        ("first", 0, 1),
        ("last", last, 1),
        ("run", RUN_LBA, RUN_BLOCKS),
        ("long run", LONG_LBA, LONG_BLOCKS),
    ];

    let read = |store: &mut Store<State>, tag: &[u8; 4], what: &str, lba: u64, count: u32| {
        let call = format!("read {what}");
        set_deadline(store, limit);
        let got = match blk.call_read(&mut *store, lba, count) {
            Err(e) => return Err(call_failed("read", limit, e)),
            Ok(Err(msg)) => return Err(driver_error(&call, &msg)),
            Ok(Ok(d)) => d,
        };
        compare(
            &call,
            lba,
            count,
            block_size,
            &pattern(tag, lba, count, block_size),
            &got,
        )
    };

    // The seed, as the image was created.
    for (what, lba, count) in places {
        read(store, SEED, what, lba, count)?;
    }
    // New data, flushed.
    for (what, lba, count) in places {
        set_deadline(store, limit);
        match blk.call_write(&mut *store, lba, &pattern(DATA, lba, count, block_size)) {
            Err(e) => return Err(call_failed("write", limit, e)),
            Ok(Err(msg)) => return Err(driver_error(&format!("write {what}"), &msg)),
            Ok(Ok(())) => emit(
                "block-written",
                &[
                    ("call", Value::Str(&format!("write {what}"))),
                    ("lba", Value::U64(lba)),
                    ("count", Value::U64(count.into())),
                ],
            ),
        }
    }
    set_deadline(store, limit);
    match blk.call_flush(&mut *store) {
        Err(e) => return Err(call_failed("flush", limit, e)),
        Ok(Err(msg)) => return Err(driver_error("flush", &msg)),
        Ok(Ok(())) => emit("block-flushed", &[]),
    }
    // Read back.
    for (what, lba, count) in places {
        read(store, DATA, what, lba, count)?;
    }

    // The error contract. Each call must return a non-empty error.
    let bs = block_size as usize;
    let invalid_reads: [(&str, u64, u32); 4] = [
        ("read at capacity", blocks, 1),
        ("read across the end", last, 2),
        ("read where lba + count overflows", u64::MAX, 1),
        ("read of 0 blocks", 0, 0),
    ];
    for (what, lba, count) in invalid_reads {
        set_deadline(store, limit);
        let r = blk
            .call_read(&mut *store, lba, count)
            .map_err(|e| call_failed("read", limit, e))?;
        let accepted = r
            .as_ref()
            .ok()
            .map(|d| format!("ok with {} bytes", d.len()));
        expect_error(what, &format!("read({lba}, {count})"), accepted, r.err())?;
    }
    let invalid_writes: [(&str, u64, Vec<u8>); 3] = [
        (
            "write across the end",
            last,
            pattern(BADW, last, 2, block_size),
        ),
        (
            "write of part of a block",
            SHORT_WRITE_LBA,
            pattern(BADW, SHORT_WRITE_LBA, 1, block_size)[..100.min(bs)].to_vec(),
        ),
        ("write of nothing", 0, Vec::new()),
    ];
    for (what, lba, data) in invalid_writes {
        set_deadline(store, limit);
        let r = blk
            .call_write(&mut *store, lba, &data)
            .map_err(|e| call_failed("write", limit, e))?;
        let accepted = r.is_ok().then(|| "ok".to_string());
        expect_error(
            what,
            &format!("write({lba}, {} bytes)", data.len()),
            accepted,
            r.err(),
        )?;
    }
    // Still working, and the write across the end wrote nothing.
    read(store, DATA, "last after the errors", last, 1)?;
    read(store, DATA, "first after the errors", 0, 1)?;
    recovery(store, blk, block_size, "baseline")?;
    Ok(())
}

/// The recovery check after a hostile phase (docs/design/robust-block.md):
/// write the RCVR pattern to RECOVERY_BLOCKS blocks from RECOVERY_LBA,
/// flush, and read it back exactly. Each call gets the usual limit; a
/// driver still reporting errors here has not recovered.
fn recovery(
    store: &mut Store<State>,
    blk: &Block,
    block_size: u32,
    after: &str,
) -> Result<(), &'static str> {
    let limit = CALL_LIMIT;
    let want = pattern(RCVR, RECOVERY_LBA, RECOVERY_BLOCKS, block_size);
    let fail = |what: &str, got: &str| {
        emit(
            "block-recovery",
            &[
                ("after", Value::Str(after)),
                ("result", Value::Str("fail")),
                (
                    "expected",
                    Value::Str(&format!(
                        "write, flush and read back {RECOVERY_BLOCKS} blocks from LBA {RECOVERY_LBA} exactly"
                    )),
                ),
                ("got", Value::Str(&format!("{what}: {got}"))),
            ],
        );
        "block-recovery"
    };
    set_deadline(store, limit);
    match blk.call_write(&mut *store, RECOVERY_LBA, &want) {
        Err(e) => return Err(call_failed("write", limit, e)),
        Ok(Err(msg)) => return Err(fail("write", &msg)),
        Ok(Ok(())) => {}
    }
    set_deadline(store, limit);
    match blk.call_flush(&mut *store) {
        Err(e) => return Err(call_failed("flush", limit, e)),
        Ok(Err(msg)) => return Err(fail("flush", &msg)),
        Ok(Ok(())) => {}
    }
    set_deadline(store, limit);
    let got = match blk.call_read(&mut *store, RECOVERY_LBA, RECOVERY_BLOCKS) {
        Err(e) => return Err(call_failed("read", limit, e)),
        Ok(Err(msg)) => return Err(fail("read", &msg)),
        Ok(Ok(d)) => d,
    };
    if got != want {
        let at = got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(0);
        return Err(fail(
            "read back",
            &format!("{} bytes, first difference at byte {at}", got.len()),
        ));
    }
    emit(
        "block-recovery",
        &[("after", Value::Str(after)), ("result", Value::Str("pass"))],
    );
    Ok(())
}

/// A call that must fail: fail the run if it succeeded or if its error
/// says nothing.
fn expect_error(
    what: &str,
    call: &str,
    accepted: Option<String>,
    error: Option<String>,
) -> Result<(), &'static str> {
    match (accepted, error) {
        (Some(got), _) => {
            emit(
                "block-accepted-invalid",
                &[
                    ("check", Value::Str(what)),
                    ("call", Value::Str(call)),
                    (
                        "expected",
                        Value::Str("an error (portals/wit, interface block)"),
                    ),
                    ("got", Value::Str(&got)),
                ],
            );
            Err("block-accepted-invalid")
        }
        (None, Some(msg)) if msg.trim().is_empty() => {
            emit(
                "block-error-unreadable",
                &[
                    ("check", Value::Str(what)),
                    ("call", Value::Str(call)),
                    (
                        "expected",
                        Value::Str("an error saying what was attempted, expected and got (rule 3)"),
                    ),
                    ("got", Value::Str("an empty error")),
                ],
            );
            Err("block-error-unreadable")
        }
        (None, msg) => {
            emit(
                "block-error-ok",
                &[
                    ("check", Value::Str(what)),
                    ("call", Value::Str(call)),
                    ("error", Value::Str(msg.as_deref().unwrap_or(""))),
                ],
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_records() {
        let p = pattern(SEED, 0x0102, 2, 512);
        assert_eq!(p.len(), 1024);
        assert_eq!(&p[0..4], b"SEED");
        assert_eq!(&p[4..12], &0x0102u64.to_le_bytes());
        assert_eq!(&p[12..16], &0u32.to_le_bytes());
        assert_eq!(&p[16 + 12..32], &1u32.to_le_bytes());
        // Second block: next LBA, records count from 0 again.
        assert_eq!(&p[512 + 4..512 + 12], &0x0103u64.to_le_bytes());
        assert_eq!(&p[512 + 12..512 + 16], &0u32.to_le_bytes());
        assert_ne!(pattern(SEED, 5, 1, 512), pattern(DATA, 5, 1, 512));
    }
}
