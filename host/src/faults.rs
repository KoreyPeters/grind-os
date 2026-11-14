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
//! Faults are drawn from a seeded generator, and every one is logged with
//! its time (`t_ms` from the start of the run, the same clock as the
//! driver's log lines), so a failure can be lined up with the fault that
//! caused it. The one exception is a window that hits every register read
//! (a device gone): those are counted per register and summarised when the
//! window closes (`fault-window-end`), since each would say the same thing. The windows are timed, so a replay with the
//! same seed injects the same kinds at the same rates, not the very same
//! calls.

use grind_shim::log::{emit, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Fault kinds, as bits in the control frame.
pub const KIND_REGISTERS: u8 = 1;
pub const KIND_DEVICE_DATA: u8 = 2;
pub const KIND_SLOW: u8 = 4;
/// NVMe: one byte of a completion entry the driver receives (result, SQ
/// head, command ID or status) changed.
pub const KIND_COMPLETIONS: u8 = 8;
/// NVMe: the device gone: every nvme portal call fails.
pub const KIND_GONE: u8 = 16;

/// Bytes of a 16-byte NVMe completion a device writes and a driver must
/// check: the command result (0-3), SQ head (8-9), command ID (12-13) and
/// status (14-15) (FreeBSD nvme.h:926-940).
const COMPLETION_FIELDS: [usize; 10] = [0, 1, 2, 3, 8, 9, 12, 13, 14, 15];

/// How long a slowed call waits.
const SLOW: Duration = Duration::from_millis(2);

/// One fault window: until when, which kinds, and how often (per mille of
/// the calls each kind applies to).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub until: Instant,
    pub kinds: u8,
    pub per_mille: u16,
}

/// What one window injected, for its summary.
#[derive(Default)]
struct WindowCounts {
    registers: u64,
    device_data: u64,
    slow: u64,
    completions: u64,
    gone: u64,
    /// Register faults per offset.
    by_register: BTreeMap<u32, u64>,
}

/// The fault state of one run.
pub struct Faults {
    rng: u64,
    /// The run's start, for `t_ms` (the host's clock portal uses the same).
    started: Instant,
    window: Option<Window>,
    counts: WindowCounts,
    /// `--fail-allocation N`: fail the Nth DMA allocation (1-based).
    fail_allocation: Option<u32>,
    allocations: u32,
    pub injected_registers: u64,
    pub injected_device_data: u64,
    pub injected_slow: u64,
    pub injected_allocation: u64,
    pub injected_completions: u64,
    pub injected_gone: u64,
    /// `--fault-identify`: Identify Namespace data reports an impossible
    /// format, for every read of the buffer last named by an Identify
    /// Namespace command (`identify_ns`).
    identify_nonsense: bool,
    identify_ns: Option<u64>,
}

impl Faults {
    pub fn new(
        seed: u64,
        fail_allocation: Option<u32>,
        identify_nonsense: bool,
        started: Instant,
    ) -> Faults {
        Faults {
            // xorshift64 must not start at zero.
            rng: seed.max(1),
            started,
            window: None,
            counts: WindowCounts::default(),
            fail_allocation,
            allocations: 0,
            injected_registers: 0,
            injected_device_data: 0,
            injected_slow: 0,
            injected_allocation: 0,
            injected_completions: 0,
            injected_gone: 0,
            identify_nonsense,
            identify_ns: None,
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

    fn t_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Open a window of `ms` milliseconds (closing any open one first).
    pub fn open(&mut self, ms: u32, kinds: u8, per_mille: u16) {
        self.close();
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
                ("completions", Value::Bool(kinds & KIND_COMPLETIONS != 0)),
                ("gone", Value::Bool(kinds & KIND_GONE != 0)),
                ("per_mille", Value::U64(w.per_mille.into())),
                ("t_ms", Value::U64(self.t_ms())),
            ],
        );
    }

    /// End the open window, if any, and log what it injected.
    pub fn close(&mut self) {
        if self.window.take().is_none() {
            return;
        }
        let c = std::mem::take(&mut self.counts);
        let by_register: Vec<String> = c
            .by_register
            .iter()
            .map(|(off, n)| format!("0x{off:x}:{n}"))
            .collect();
        emit(
            "fault-window-end",
            &[
                ("t_ms", Value::U64(self.t_ms())),
                ("registers", Value::U64(c.registers)),
                ("device_data", Value::U64(c.device_data)),
                ("slow", Value::U64(c.slow)),
                ("completions", Value::U64(c.completions)),
                ("gone", Value::U64(c.gone)),
                (
                    "register_faults_by_offset",
                    Value::Str(&by_register.join(" ")),
                ),
            ],
        );
    }

    /// Whether a fault of `kind` hits this call.
    fn hit(&mut self, kind: u8) -> bool {
        let Some(w) = self.window else { return false };
        if Instant::now() >= w.until {
            self.close();
            return false;
        }
        w.kinds & kind != 0 && self.next() % 1000 < u64::from(w.per_mille)
    }

    fn log(&self, what: &str, fields: &[(&str, Value)]) {
        let mut f = vec![
            ("t_ms", Value::U64(self.t_ms())),
            ("kind", Value::Str(what)),
        ];
        f.extend_from_slice(fields);
        emit("fault-injected", &f);
    }

    /// A register read: `Some(all-ones)` to return instead of the device's
    /// value, or `None` to read the device.
    pub fn register_read(&mut self, op: &str, offset: u32, width: u32) -> Option<u32> {
        self.slow();
        if !self.hit(KIND_REGISTERS) {
            return None;
        }
        self.injected_registers += 1;
        self.counts.registers += 1;
        *self.counts.by_register.entry(offset).or_default() += 1;
        // A window that hits every read is a device gone: summarised at
        // its end instead of one line per read.
        if self.window.is_some_and(|w| w.per_mille < 1000) {
            self.log(
                "register-all-ones",
                &[
                    ("op", Value::Str(op)),
                    ("offset", Value::Hex(offset.into())),
                ],
            );
        }
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
        self.counts.device_data += 1;
        self.log(
            "device-data",
            &[
                ("iova", Value::Hex(iova)),
                ("offset", Value::Hex(u64::from(offset) + at as u64)),
                ("was", Value::Hex(old.into())),
                ("now", Value::Hex(data[at].into())),
            ],
        );
    }

    /// An nvme portal call: true if the device is gone for it, and the call
    /// must fail. Counted, and logged unless the window hits every call.
    pub fn gone(&mut self, op: &str) -> bool {
        if !self.hit(KIND_GONE) {
            return false;
        }
        self.injected_gone += 1;
        self.counts.gone += 1;
        if self.window.is_some_and(|w| w.per_mille < 1000) {
            self.log("nvme-gone", &[("op", Value::Str(op))]);
        }
        true
    }

    /// A completion entry the driver is about to receive: maybe change one
    /// of the bytes a driver must check.
    pub fn completion(&mut self, op: &str, cpl: &mut [u8]) {
        if cpl.len() != 16 || !self.hit(KIND_COMPLETIONS) {
            return;
        }
        let at = COMPLETION_FIELDS[(self.next() % COMPLETION_FIELDS.len() as u64) as usize];
        let old = cpl[at];
        cpl[at] ^= (self.next() % 255 + 1) as u8;
        self.injected_completions += 1;
        self.counts.completions += 1;
        self.log(
            "nvme-completion",
            &[
                ("op", Value::Str(op)),
                ("byte", Value::U64(at as u64)),
                ("was", Value::Hex(old.into())),
                ("now", Value::Hex(cpl[at].into())),
            ],
        );
    }

    /// An admin command: remember the buffer of an Identify Namespace (CNS
    /// 0), whose data `--fault-identify` makes impossible.
    pub fn admin_command(&mut self, command: &[u8]) {
        if command.len() != 64 || command[0] != 0x06 {
            return;
        }
        let prp1 = u64::from_le_bytes(command[24..32].try_into().expect("8 bytes"));
        let cns = u32::from_le_bytes(command[40..44].try_into().expect("4 bytes"));
        if cns == 0 {
            self.identify_ns = Some(prp1);
        } else if self.identify_ns == Some(prp1) {
            self.identify_ns = None;
        }
    }

    /// `--fault-identify`: a read of the Identify Namespace buffer reports
    /// a format index (FLBAS, byte 26) beyond the formats there are (NLBAF,
    /// byte 25): an impossible namespace (FreeBSD nvme.h:1586-1695).
    pub fn identify_data(&mut self, iova: u64, offset: u32, data: &mut [u8]) {
        if !self.identify_nonsense || self.identify_ns != Some(iova) {
            return;
        }
        let mut changed = false;
        for (byte, value) in [(25usize, 0u8), (26, 0x0f)] {
            if let Some(i) = byte
                .checked_sub(offset as usize)
                .filter(|i| *i < data.len())
            {
                data[i] = value;
                changed = true;
            }
        }
        if changed {
            self.log(
                "identify-nonsense",
                &[
                    ("iova", Value::Hex(iova)),
                    ("nlbaf", Value::U64(0)),
                    ("flbas", Value::Hex(0x0f)),
                ],
            );
        }
    }

    fn slow(&mut self) {
        if self.hit(KIND_SLOW) {
            self.injected_slow += 1;
            self.counts.slow += 1;
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
                ("t_ms", Value::U64(self.t_ms())),
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
        let mut f = Faults::new(1, None, false, Instant::now());
        for _ in 0..1000 {
            assert_eq!(f.register_read("read32", 0, 4), None);
        }
        let mut d = vec![7u8; 64];
        f.device_data(0, 0, &mut d);
        assert_eq!(d, vec![7u8; 64]);
    }

    #[test]
    fn a_window_injects_at_about_its_rate_and_ends() {
        let mut f = Faults::new(42, None, false, Instant::now());
        f.open(60_000, KIND_REGISTERS, 200);
        let hits = (0..10_000)
            .filter(|_| f.register_read("read32", 0x8, 4).is_some())
            .count();
        assert!((1500..2500).contains(&hits), "about 20%: {hits}");
        let mut short = Faults::new(42, None, false, Instant::now());
        short.open(0, KIND_REGISTERS, 1000);
        assert_eq!(
            short.register_read("read32", 0, 4),
            None,
            "a 0 ms window is over"
        );
    }

    #[test]
    fn all_ones_fits_the_width() {
        let mut f = Faults::new(3, None, false, Instant::now());
        f.open(60_000, KIND_REGISTERS, 1000);
        assert_eq!(f.register_read("read8", 0, 1), Some(0xff));
        assert_eq!(f.register_read("read16", 0, 2), Some(0xffff));
        assert_eq!(f.register_read("read32", 0, 4), Some(0xffff_ffff));
    }

    #[test]
    fn device_data_changes_exactly_one_byte() {
        let mut f = Faults::new(9, None, false, Instant::now());
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
            let mut f = Faults::new(seed, None, false, Instant::now());
            f.open(60_000, KIND_REGISTERS, 300);
            (0..200)
                .map(|_| f.register_read("read32", 0, 4).is_some())
                .collect::<Vec<_>>()
        };
        assert_eq!(run(5), run(5));
        assert_ne!(run(5), run(6));
    }

    #[test]
    fn a_window_counts_its_faults_by_register_and_resets_on_close() {
        let mut f = Faults::new(4, None, false, Instant::now());
        f.open(60_000, KIND_REGISTERS, 1000);
        for _ in 0..3 {
            f.register_read("read32", 0x8, 4);
        }
        f.register_read("read16", 0x3e, 2);
        assert_eq!(f.counts.registers, 4);
        assert_eq!(f.counts.by_register.get(&0x8), Some(&3));
        assert_eq!(f.counts.by_register.get(&0x3e), Some(&1));
        f.close();
        assert!(f.window.is_none());
        assert_eq!(f.counts.registers, 0, "the next window starts afresh");
        assert_eq!(f.injected_registers, 4, "the run total is kept");
        // Opening a new window closes the old one first.
        f.open(60_000, KIND_REGISTERS, 1000);
        f.register_read("read32", 0, 4);
        f.open(60_000, KIND_DEVICE_DATA, 1000);
        assert_eq!(f.counts.registers, 0);
    }

    #[test]
    fn completion_faults_touch_only_checked_fields() {
        let mut f = Faults::new(11, None, false, Instant::now());
        f.open(60_000, KIND_COMPLETIONS, 1000);
        for _ in 0..200 {
            let mut c = [0u8; 16];
            f.completion("complete", &mut c);
            let changed: Vec<usize> = (0..16).filter(|i| c[*i] != 0).collect();
            assert_eq!(changed.len(), 1);
            assert!(COMPLETION_FIELDS.contains(&changed[0]), "{changed:?}");
        }
        assert!(!f.gone("submit"), "not a gone window");
    }

    #[test]
    fn identify_nonsense_only_for_the_namespace_buffer() {
        let mut f = Faults::new(1, None, true, Instant::now());
        let mut cmd = [0u8; 64];
        cmd[0] = 0x06;
        cmd[24..32].copy_from_slice(&0x1000u64.to_le_bytes());
        f.admin_command(&cmd); // CNS 0: namespace
        let mut d = vec![1u8; 4096];
        f.identify_data(0x1000, 0, &mut d);
        assert_eq!((d[25], d[26]), (0, 0x0f));
        let mut other = vec![1u8; 4096];
        f.identify_data(0x2000, 0, &mut other);
        assert_eq!((other[25], other[26]), (1, 1), "another buffer");
        cmd[40] = 1; // CNS 1 (controller) into the same buffer
        f.admin_command(&cmd);
        let mut ctrl = vec![1u8; 4096];
        f.identify_data(0x1000, 0, &mut ctrl);
        assert_eq!((ctrl[25], ctrl[26]), (1, 1), "no longer namespace data");
    }

    #[test]
    fn fail_allocation_fails_exactly_the_nth() {
        let mut f = Faults::new(1, Some(2), false, Instant::now());
        assert!(!f.allocation("allocate", 4096));
        assert!(f.allocation("allocate", 4096));
        assert!(!f.allocation("allocate", 4096));
    }
}
