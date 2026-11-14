// SPDX-License-Identifier: AGPL-3.0-or-later

//! Device fault injection for the NIC robustness contract
//! (docs/design/robust-nic.md).
//!
//! During a fault window, opened by the test's peer with a control frame,
//! the host changes what the *driver* sees: some register reads return
//! all-ones (what a removed or wedged PCIe device returns), some DMA buffer
//! reads come back with a byte changed (a device writing nonsense into a
//! descriptor or header), and some calls are slowed. Writes, and anything
//! that reaches the device, are never changed, so rule 10 is untouched.
//! Separately, `--fail-allocation N` makes the driver's Nth DMA allocation
//! fail.
//!
//! Faults are drawn from a seeded generator, and every one is logged (the
//! first few of each kind in full, then counted), so a failure says what
//! was injected and where. The windows are timed, so a replay with the
//! same seed injects the same kinds at the same rates, not the very same
//! calls.

use grind_shim::log::{emit, Value};
use std::time::{Duration, Instant};

/// Fault kinds, as bits in the control frame.
pub const KIND_REGISTERS: u8 = 1;
pub const KIND_DEVICE_DATA: u8 = 2;
pub const KIND_SLOW: u8 = 4;

/// How long a slowed call waits.
const SLOW: Duration = Duration::from_millis(2);
/// Injected faults logged in full per kind; the rest are counted.
const LOG_CAP: u64 = 10;

/// One fault window: until when, which kinds, and how often (per mille of
/// the calls each kind applies to).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub until: Instant,
    pub kinds: u8,
    pub per_mille: u16,
}

/// The fault state of one run.
pub struct Faults {
    rng: u64,
    window: Option<Window>,
    /// `--fail-allocation N`: fail the Nth DMA allocation (1-based).
    fail_allocation: Option<u32>,
    allocations: u32,
    pub injected_registers: u64,
    pub injected_device_data: u64,
    pub injected_slow: u64,
    pub injected_allocation: u64,
}

impl Faults {
    pub fn new(seed: u64, fail_allocation: Option<u32>) -> Faults {
        Faults {
            // xorshift64 must not start at zero.
            rng: seed.max(1),
            window: None,
            fail_allocation,
            allocations: 0,
            injected_registers: 0,
            injected_device_data: 0,
            injected_slow: 0,
            injected_allocation: 0,
        }
    }

    /// xorshift64 (Marsaglia, 2003): small, fast, deterministic.
    fn next(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Open a window of `ms` milliseconds.
    pub fn open(&mut self, ms: u32, kinds: u8, per_mille: u16) {
        let w = Window {
            until: Instant::now() + Duration::from_millis(ms.into()),
            kinds,
            per_mille: per_mille.min(1000),
        };
        self.window = Some(w);
        emit(
            "fault-window",
            &[
                ("ms", Value::U64(ms.into())),
                ("registers", Value::Bool(kinds & KIND_REGISTERS != 0)),
                ("device_data", Value::Bool(kinds & KIND_DEVICE_DATA != 0)),
                ("slow", Value::Bool(kinds & KIND_SLOW != 0)),
                ("per_mille", Value::U64(w.per_mille.into())),
            ],
        );
    }

    /// Whether a fault of `kind` hits this call.
    fn hit(&mut self, kind: u8) -> bool {
        let Some(w) = self.window else { return false };
        if Instant::now() >= w.until {
            self.window = None;
            return false;
        }
        w.kinds & kind != 0 && self.next() % 1000 < u64::from(w.per_mille)
    }

    fn log(n: u64, what: &str, fields: &[(&str, Value)]) {
        if n <= LOG_CAP {
            let mut f = vec![("kind", Value::Str(what))];
            f.extend_from_slice(fields);
            emit("fault-injected", &f);
        }
    }

    /// A register read: `Some(all-ones)` to return instead of the device's
    /// value, or `None` to read the device.
    pub fn register_read(&mut self, op: &str, offset: u32, width: u32) -> Option<u32> {
        self.slow();
        if !self.hit(KIND_REGISTERS) {
            return None;
        }
        self.injected_registers += 1;
        Self::log(
            self.injected_registers,
            "register-all-ones",
            &[
                ("op", Value::Str(op)),
                ("offset", Value::Hex(offset.into())),
            ],
        );
        Some(if width == 4 {
            u32::MAX
        } else {
            (1 << (8 * width)) - 1
        })
    }

    /// A DMA buffer read: maybe change one byte of what the driver sees.
    pub fn device_data(&mut self, iova: u64, offset: u32, data: &mut [u8]) {
        self.slow();
        if data.is_empty() || !self.hit(KIND_DEVICE_DATA) {
            return;
        }
        let at = (self.next() % data.len() as u64) as usize;
        let old = data[at];
        // Never zero: the byte always changes.
        data[at] ^= (self.next() % 255 + 1) as u8;
        self.injected_device_data += 1;
        Self::log(
            self.injected_device_data,
            "device-data",
            &[
                ("iova", Value::Hex(iova)),
                ("offset", Value::Hex(u64::from(offset) + at as u64)),
                ("was", Value::Hex(old.into())),
                ("now", Value::Hex(data[at].into())),
            ],
        );
    }

    fn slow(&mut self) {
        if self.hit(KIND_SLOW) {
            self.injected_slow += 1;
            std::thread::sleep(SLOW);
        }
    }

    /// A DMA allocation: true if this one must fail (`--fail-allocation`).
    pub fn allocation(&mut self, op: &str, size: u32) -> bool {
        self.allocations += 1;
        if self.fail_allocation != Some(self.allocations) {
            return false;
        }
        self.injected_allocation += 1;
        emit(
            "fault-injected",
            &[
                ("kind", Value::Str("allocation")),
                ("op", Value::Str(op)),
                ("size", Value::U64(size.into())),
                ("allocation", Value::U64(self.allocations.into())),
            ],
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_faults_outside_a_window() {
        let mut f = Faults::new(1, None);
        for _ in 0..1000 {
            assert_eq!(f.register_read("read32", 0, 4), None);
        }
        let mut d = vec![7u8; 64];
        f.device_data(0, 0, &mut d);
        assert_eq!(d, vec![7u8; 64]);
    }

    #[test]
    fn a_window_injects_at_about_its_rate_and_ends() {
        let mut f = Faults::new(42, None);
        f.open(60_000, KIND_REGISTERS, 200);
        let hits = (0..10_000)
            .filter(|_| f.register_read("read32", 0x8, 4).is_some())
            .count();
        assert!((1500..2500).contains(&hits), "about 20%: {hits}");
        let mut short = Faults::new(42, None);
        short.open(0, KIND_REGISTERS, 1000);
        assert_eq!(
            short.register_read("read32", 0, 4),
            None,
            "a 0 ms window is over"
        );
    }

    #[test]
    fn all_ones_fits_the_width() {
        let mut f = Faults::new(3, None);
        f.open(60_000, KIND_REGISTERS, 1000);
        assert_eq!(f.register_read("read8", 0, 1), Some(0xff));
        assert_eq!(f.register_read("read16", 0, 2), Some(0xffff));
        assert_eq!(f.register_read("read32", 0, 4), Some(0xffff_ffff));
    }

    #[test]
    fn device_data_changes_exactly_one_byte() {
        let mut f = Faults::new(9, None);
        f.open(60_000, KIND_DEVICE_DATA, 1000);
        let mut d = vec![0x55u8; 32];
        f.device_data(0x1000, 0, &mut d);
        assert_eq!(d.iter().filter(|b| **b != 0x55).count(), 1);
        // Registers are not touched by a device-data window.
        assert_eq!(f.register_read("read32", 0, 4), None);
    }

    #[test]
    fn the_same_seed_gives_the_same_faults() {
        let run = |seed| {
            let mut f = Faults::new(seed, None);
            f.open(60_000, KIND_REGISTERS, 300);
            (0..200)
                .map(|_| f.register_read("read32", 0, 4).is_some())
                .collect::<Vec<_>>()
        };
        assert_eq!(run(5), run(5));
        assert_ne!(run(5), run(6));
    }

    #[test]
    fn fail_allocation_fails_exactly_the_nth() {
        let mut f = Faults::new(1, Some(2));
        assert!(!f.allocation("allocate", 4096));
        assert!(f.allocation("allocate", 4096));
        assert!(!f.allocation("allocate", 4096));
    }
}
