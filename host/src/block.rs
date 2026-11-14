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
    /// `--block-device-errors`: the disk has blkdebug's bad blocks
    /// (tests/block), so run the device-errors phase.
    pub device_errors: bool,
    /// `--block-power-loss`: end with the power-loss phase; tests/block's
    /// director cuts the power at its marker.
    pub power_loss: bool,
}

/// The power-loss phase: blocks written and flushed before the cut
/// (PWRA), and blocks written after the flush, before the cut (PWRB).
pub const PWRA: &[u8; 4] = b"PWRA";
pub const PWRB: &[u8; 4] = b"PWRB";
pub const POWER_FLUSHED_LBA: u64 = 6000;
pub const POWER_UNFLUSHED_LBA: u64 = 6100;
pub const POWER_BLOCKS: u32 = 16;
/// How long the host waits for the power cut before reporting that it
/// never came. Generous: the director reads the marker from the serial
/// console, which can lag by many seconds behind a burst of log lines.
const POWER_CUT_WAIT_MS: u64 = 60_000;

/// Blocks tests/block makes fail with real I/O errors (QEMU's blkdebug):
/// reads of BAD_READ_LBA always, writes to BAD_WRITE_LBA always, and the
/// first read of TRANSIENT_LBA only.
pub const BAD_READ_LBA: u64 = 3000;
pub const BAD_WRITE_LBA: u64 = 3100;
pub const TRANSIENT_LBA: u64 = 3200;

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

    // Nonsense in what the device returns, then the device gone and back
    // (host/src/faults.rs: only what the driver sees changes).
    faulty_reads(
        store,
        blk,
        block_size,
        "completion-nonsense",
        crate::faults::KIND_COMPLETIONS,
        150,
    )?;
    recovery(store, blk, block_size, "completion-nonsense")?;
    faulty_reads(
        store,
        blk,
        block_size,
        "device-gone",
        crate::faults::KIND_REGISTERS | crate::faults::KIND_GONE,
        1000,
    )?;
    recovery(store, blk, block_size, "device-gone")?;
    if expect.device_errors {
        slow_device(store, blk, block_size)?;
        recovery(store, blk, block_size, "slow-device")?;
    }

    if expect.device_errors {
        device_errors(store, blk, block_size)?;
        recovery(store, blk, block_size, "device-errors")?;
    }
    if expect.power_loss {
        power_loss(store, blk, block_size)?;
    }
    Ok(())
}

/// Phase, always last: power loss after a flush (docs/design/robust-block.md).
/// Write PWRA and flush, write PWRB without flushing, then print the
/// marker and wait: tests/block's director kills the write-back disk
/// (losing what was not flushed) and stops the VM. tests/block/image.py
/// then requires PWRA everywhere it was written, and old or new contents
/// (SEED or PWRB) where PWRB was. This host never reports on the phase
/// itself; if the power is still on after POWER_CUT_WAIT_MS, it fails.
fn power_loss(store: &mut Store<State>, blk: &Block, block_size: u32) -> Result<(), &'static str> {
    let limit = CALL_LIMIT;
    emit(
        "block-phase",
        &[("phase", Value::Str("power-loss-prepare"))],
    );
    for (tag, lba, then_flush) in [
        (PWRA, POWER_FLUSHED_LBA, true),
        (PWRB, POWER_UNFLUSHED_LBA, false),
    ] {
        set_deadline(store, limit);
        match blk.call_write(
            &mut *store,
            lba,
            &pattern(tag, lba, POWER_BLOCKS, block_size),
        ) {
            Err(e) => return Err(call_failed("write", limit, e)),
            Ok(Err(msg)) => return Err(driver_error("write before the power loss", &msg)),
            Ok(Ok(())) => {}
        }
        if then_flush {
            set_deadline(store, limit);
            match blk.call_flush(&mut *store) {
                Err(e) => return Err(call_failed("flush", limit, e)),
                Ok(Err(msg)) => return Err(driver_error("flush before the power loss", &msg)),
                Ok(Ok(())) => {}
            }
        }
    }
    emit(
        "block-phase",
        &[
            ("phase", Value::Str("power-loss")),
            (
                "flushed",
                Value::Str(&format!("{POWER_FLUSHED_LBA}+{POWER_BLOCKS}")),
            ),
            (
                "unflushed",
                Value::Str(&format!("{POWER_UNFLUSHED_LBA}+{POWER_BLOCKS}")),
            ),
        ],
    );
    std::thread::sleep(Duration::from_millis(POWER_CUT_WAIT_MS));
    emit(
        "block-power-not-cut",
        &[
            (
                "expected",
                Value::Str("tests/block's director cuts the power at the power-loss marker"),
            ),
            (
                "got",
                Value::Str(&format!("still running {POWER_CUT_WAIT_MS} ms later")),
            ),
        ],
    );
    Err("block-power-not-cut")
}

/// Blocks the faulty-read phases read: never written, so they hold the seed.
const FAULTY_LBA: u64 = 5000;
const FAULTY_SPAN: u64 = 64;
/// How long a fault window lasts.
const FAULT_WINDOW_MS: u32 = 1500;

/// Phase: open a fault window and keep reading known blocks while it lasts.
/// A read may fail (with a readable error) but must never return wrong
/// data; every call must return within the limit.
fn faulty_reads(
    store: &mut Store<State>,
    blk: &Block,
    block_size: u32,
    phase: &'static str,
    kinds: u8,
    per_mille: u16,
) -> Result<(), &'static str> {
    let limit = CALL_LIMIT;
    emit("block-phase", &[("phase", Value::Str(phase))]);
    store
        .data_mut()
        .faults
        .open(FAULT_WINDOW_MS, kinds, per_mille);
    let end = std::time::Instant::now() + Duration::from_millis(FAULT_WINDOW_MS.into());
    let (mut ok, mut errors, mut i) = (0u64, 0u64, 0u64);
    while std::time::Instant::now() < end {
        let lba = FAULTY_LBA + (i * 7) % FAULTY_SPAN;
        let count = 1 + (i % 8) as u32;
        i += 1;
        set_deadline(store, limit);
        match blk.call_read(&mut *store, lba, count) {
            Err(e) => return Err(call_failed("read", limit, e)),
            Ok(Err(msg)) => {
                if msg.trim().is_empty() {
                    emit(
                        "block-error-unreadable",
                        &[
                            ("check", Value::Str(phase)),
                            ("call", Value::Str(&format!("read({lba}, {count})"))),
                            (
                                "expected",
                                Value::Str(
                                    "an error saying what was attempted, expected and got (rule 3)",
                                ),
                            ),
                            ("got", Value::Str("an empty error")),
                        ],
                    );
                    return Err("block-error-unreadable");
                }
                errors += 1;
            }
            Ok(Ok(got)) => {
                // Errors are allowed here; wrong answers are not. Only a
                // wrong answer is logged (in full, by compare).
                let want = pattern(SEED, lba, count, block_size);
                if got != want {
                    let _ = compare(
                        &format!("read during {phase}"),
                        lba,
                        count,
                        block_size,
                        &want,
                        &got,
                    );
                    return Err("block-wrong-answer");
                }
                ok += 1;
            }
        }
    }
    store.data_mut().faults.close();
    emit(
        "block-phase-end",
        &[
            ("phase", Value::Str(phase)),
            ("reads_ok", Value::U64(ok)),
            ("reads_failed", Value::U64(errors)),
        ],
    );
    Ok(())
}

/// How long the slow-device phase reads while tests/block/director.py has
/// the disk throttled (1 I/O per second).
const SLOW_WINDOW_MS: u64 = 3000;
/// The director acts on markers it reads from the serial log, which can lag
/// by seconds when there is a lot of output. So the phase does not assume:
/// it probes until a read is slow (throttled) or fast again (lifted), for
/// at most DIRECTOR_WAIT_MS each way.
const DIRECTOR_WAIT_MS: u64 = 15_000;
/// A read at least this slow means the throttle is in effect (1 I/O per
/// second); one at most THROTTLE_LIFTED_MS means it has been lifted.
const THROTTLED_MS: u64 = 400;
const THROTTLE_LIFTED_MS: u64 = 100;

/// Phase: a slow device (docs/design/robust-block.md). The host prints a
/// marker; tests/block/director.py, outside the VM, throttles the disk to
/// 1 I/O per second through QEMU's monitor, slower than a driver may want
/// to wait. Every call must still return within the limit (a timeout is
/// fatal), reads may fail readably but never return wrong data, and a
/// command the driver gave up on but the device completes later must not
/// confuse it: the recovery check after the phase tests that.
fn slow_device(store: &mut Store<State>, blk: &Block, block_size: u32) -> Result<(), &'static str> {
    let limit = CALL_LIMIT;
    let phase = "slow-device";
    emit("block-phase", &[("phase", Value::Str(phase))]);
    // Wait until the throttle is really in effect.
    let waited = probe_until(store, blk, |ms| ms >= THROTTLED_MS)?;
    let Some(waited) = waited else {
        emit(
            "block-throttle-not-applied",
            &[
                (
                    "expected",
                    Value::Str(&format!(
                        "a read taking at least {THROTTLED_MS} ms within {DIRECTOR_WAIT_MS} ms of the slow-device marker (tests/block/director.py throttles the disk)"
                    )),
                ),
                ("got", Value::Str("every read stayed fast: the disk was never throttled")),
            ],
        );
        return Err("block-throttle-not-applied");
    };
    emit(
        "block-throttle-seen",
        &[("after_ms", Value::U64(waited.as_millis() as u64))],
    );
    let end = std::time::Instant::now() + Duration::from_millis(SLOW_WINDOW_MS);
    let (mut ok, mut errors, mut i, mut slowest) = (0u64, 0u64, 0u64, Duration::ZERO);
    while std::time::Instant::now() < end {
        let lba = FAULTY_LBA + (i * 7) % FAULTY_SPAN;
        i += 1;
        set_deadline(store, limit);
        let began = std::time::Instant::now();
        let r = blk
            .call_read(&mut *store, lba, 1)
            .map_err(|e| call_failed("read", limit, e))?;
        slowest = slowest.max(began.elapsed());
        match r {
            Err(msg) if msg.trim().is_empty() => return Err("block-error-unreadable"),
            Err(_) => errors += 1,
            Ok(got) => {
                let want = pattern(SEED, lba, 1, block_size);
                if got != want {
                    let _ = compare("read of a slow device", lba, 1, block_size, &want, &got);
                    return Err("block-wrong-answer");
                }
                ok += 1;
            }
        }
    }
    emit(
        "block-phase-end",
        &[
            ("phase", Value::Str(phase)),
            ("reads_ok", Value::U64(ok)),
            ("reads_failed", Value::U64(errors)),
            ("slowest_call_ms", Value::U64(slowest.as_millis() as u64)),
        ],
    );
    // Wait until the throttle is really lifted (late completions arrive
    // meanwhile).
    if probe_until(store, blk, |ms| ms <= THROTTLE_LIFTED_MS)?.is_none() {
        emit(
            "block-throttle-not-lifted",
            &[
                (
                    "expected",
                    Value::Str(&format!(
                        "a read taking at most {THROTTLE_LIFTED_MS} ms within {DIRECTOR_WAIT_MS} ms of the end marker"
                    )),
                ),
                ("got", Value::Str("every read stayed slow")),
            ],
        );
        return Err("block-throttle-not-lifted");
    }
    Ok(())
}

/// Read one known block repeatedly until a read's time (in ms, success or
/// a readable error) satisfies `done`, for at most DIRECTOR_WAIT_MS.
/// Ok(Some(elapsed)) when it did; Ok(None) if it never did.
fn probe_until(
    store: &mut Store<State>,
    blk: &Block,
    done: impl Fn(u64) -> bool,
) -> Result<Option<Duration>, &'static str> {
    let limit = CALL_LIMIT;
    let began = std::time::Instant::now();
    while began.elapsed() < Duration::from_millis(DIRECTOR_WAIT_MS) {
        set_deadline(store, limit);
        let t = std::time::Instant::now();
        let r = blk
            .call_read(&mut *store, FAULTY_LBA, 1)
            .map_err(|e| call_failed("read", limit, e))?;
        if let Err(msg) = &r {
            if msg.trim().is_empty() {
                return Err("block-error-unreadable");
            }
        }
        if done(t.elapsed().as_millis() as u64) {
            return Ok(Some(began.elapsed()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(None)
}

/// Phase: errors the device really reports (docs/design/robust-block.md).
/// Bad blocks must give readable errors; a transient error must be retried
/// to success; the blocks around them must keep working.
fn device_errors(
    store: &mut Store<State>,
    blk: &Block,
    block_size: u32,
) -> Result<(), &'static str> {
    let limit = CALL_LIMIT;
    emit("block-phase", &[("phase", Value::Str("device-errors"))]);
    let failing_reads: [(&str, u64, u32); 2] = [
        ("read of a bad block", BAD_READ_LBA, 1),
        ("read spanning a bad block", BAD_READ_LBA - 4, 8),
    ];
    for (what, lba, count) in failing_reads {
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
    set_deadline(store, limit);
    let data = pattern(BADW, BAD_WRITE_LBA, 1, block_size);
    let r = blk
        .call_write(&mut *store, BAD_WRITE_LBA, &data)
        .map_err(|e| call_failed("write", limit, e))?;
    let accepted = r.is_ok().then(|| "ok".to_string());
    expect_error(
        "write of a bad block",
        &format!("write({BAD_WRITE_LBA}, 1 block)"),
        accepted,
        r.err(),
    )?;

    // A transient error: the first read fails, a retry succeeds. The call
    // must return the right data.
    set_deadline(store, limit);
    let call = "read of a block that fails once (a transient error)";
    let got = match blk.call_read(&mut *store, TRANSIENT_LBA, 1) {
        Err(e) => return Err(call_failed("read", limit, e)),
        Ok(Err(msg)) => {
            emit(
                "block-transient-not-retried",
                &[
                    ("call", Value::Str(call)),
                    (
                        "expected",
                        Value::Str("the seed, after a bounded retry (robustness contract)"),
                    ),
                    ("got", Value::Str(&format!("an error: {msg}"))),
                ],
            );
            return Err("block-transient-not-retried");
        }
        Ok(Ok(d)) => d,
    };
    compare(
        call,
        TRANSIENT_LBA,
        1,
        block_size,
        &pattern(SEED, TRANSIENT_LBA, 1, block_size),
        &got,
    )?;

    // The neighbours still work.
    for lba in [BAD_READ_LBA + 1, BAD_WRITE_LBA + 1] {
        set_deadline(store, limit);
        let got = match blk.call_read(&mut *store, lba, 1) {
            Err(e) => return Err(call_failed("read", limit, e)),
            Ok(Err(msg)) => return Err(driver_error("read next to a bad block", &msg)),
            Ok(Ok(d)) => d,
        };
        compare(
            "read next to a bad block",
            lba,
            1,
            block_size,
            &pattern(SEED, lba, 1, block_size),
            &got,
        )?;
    }
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
