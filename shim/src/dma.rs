// SPDX-License-Identifier: AGPL-3.0-or-later

//! DMA buffers: pinned host memory mapped into the device's IOMMU domain.
//!
//! The driver only ever learns a buffer's IOVA (the address the device
//! uses). It reads and writes the contents through copies, so no host
//! pointer crosses the portal. The device can reach mapped buffers and
//! nothing else; a page-sized gap after each buffer makes an overrun fault
//! in the IOMMU instead of landing in the next buffer.
//!
//! Each buffer is owned either by the driver or by the shim itself (for
//! example, NVMe submission queues). The allocator remembers which live
//! ranges are the driver's, so the shim can check that every address a
//! driver hands the device lies inside the driver's own buffers (rule 10).
//! IOVAs are never reused, so a freed driver buffer's addresses can never
//! come to name shim memory.
//!
//! IOVAs come from two regions of the device's IOMMU address space, chosen
//! from the usable ranges the kernel reports ([`Regions::from_ranges`]):
//! the high region, from 4 GiB up, for ordinary buffers; and the low
//! region, from 1 MiB up to the first reserved hole below 4 GiB, for
//! devices that can only address 32 bits ([`DmaAllocator::allocate32`]).
//! IOVAs are addresses in the device's own domain, not physical memory, so
//! any page can sit at a low IOVA.

use crate::log::{emit, Value};
use crate::sys::*;
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{fence, Ordering};
use std::sync::{Arc, Mutex};

pub const PAGE: u64 = 4096;
/// Largest single buffer.
pub const MAX_BUFFER: u32 = 16 << 20;
/// The high region starts at 4 GiB, clear of the x86 MSI window at
/// 0xfee00000.
const HIGH_BASE: u64 = 1 << 32;
/// Assumed end of the high region when the kernel reports no ranges:
/// QEMU's emulated Intel IOMMU has a 39-bit address width.
const HIGH_LIMIT_ASSUMED: u64 = 1 << 39;
/// The low region starts at 1 MiB, so IOVA 0 and its neighbours (what a
/// zeroed descriptor names) are never a buffer.
const LOW_BASE: u64 = 1 << 20;
/// The low region ends at 4 GiB: a device with 32-bit addressing must
/// reach the whole buffer.
const LOW_LIMIT: u64 = 1 << 32;

/// Which region a buffer comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    High,
    Low,
}

/// The two regions, each `[start, end)`. `None` when it cannot be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Regions {
    pub high: Option<(u64, u64)>,
    pub low: Option<(u64, u64)>,
}

impl Regions {
    /// Choose the regions from the usable IOVA ranges (`[start, end]`,
    /// inclusive, as VFIO reports them). With no ranges reported, the high
    /// region is assumed as before ([4 GiB, 2^39)) and there is no low
    /// region. Each region is the part of one usable range from its base;
    /// it ends at the first reserved hole.
    pub fn from_ranges(ranges: Option<&[(u64, u64)]>) -> Regions {
        let Some(ranges) = ranges else {
            return Regions {
                high: Some((HIGH_BASE, HIGH_LIMIT_ASSUMED)),
                low: None,
            };
        };
        let containing = |base: u64, cap: u64| {
            ranges
                .iter()
                .find(|(s, e)| *s <= base && base <= *e)
                .map(|(_, e)| (base, e.saturating_add(1).min(cap)))
                .filter(|(s, e)| s < e)
        };
        Regions {
            high: containing(HIGH_BASE, u64::MAX),
            low: containing(LOW_BASE, LOW_LIMIT),
        }
    }
}

/// The usable IOVA ranges from a `VFIO_IOMMU_GET_INFO` reply
/// ([`crate::sys::iommu_info`]), or `None` if it has no IOVA-range
/// capability or does not parse.
pub fn parse_iova_ranges(reply: &[u8]) -> Option<Vec<(u64, u64)>> {
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(reply.get(at..at + 4)?.try_into().ok()?))
    };
    let u64_at = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(reply.get(at..at + 8)?.try_into().ok()?))
    };
    if u32_at(4)? & VFIO_IOMMU_INFO_CAPS == 0 {
        return None;
    }
    let mut off = u32_at(16)? as usize;
    // A chain longer than the buffer could hold is malformed.
    for _ in 0..reply.len() / 8 {
        if off == 0 {
            return None;
        }
        let id = u16::from_le_bytes(reply.get(off..off + 2)?.try_into().ok()?);
        if id == VFIO_IOMMU_TYPE1_INFO_CAP_IOVA_RANGE {
            let n = u32_at(off + 8)? as usize;
            return (0..n)
                .map(|i| Some((u64_at(off + 16 + 16 * i)?, u64_at(off + 24 + 16 * i)?)))
                .collect();
        }
        off = u32_at(off + 4)? as usize;
    }
    None
}

#[repr(C, align(4096))]
struct Page([u8; 4096]);

/// The VFIO container and group. Buffers hold a reference so the group
/// stays attached, and the mapping valid, until each buffer is unmapped.
pub(crate) struct Domain {
    pub container: File,
    /// Held, never read: keeps the group attached to the container.
    pub _group: File,
}

#[derive(Debug)]
pub enum DmaError {
    /// Zero, or larger than [`MAX_BUFFER`].
    BadSize,
    /// The allocator's total limit (or the region's IOVA space) is used up,
    /// or the region cannot be offered.
    OutOfMemory,
    /// The access does not fit inside the buffer.
    OutOfRange,
    /// Mapping failed. Not the driver's fault; already logged.
    Io(io::Error),
}

struct State {
    /// Next free IOVA and end of each region; `None` if not offered.
    high: Option<(u64, u64)>,
    low: Option<(u64, u64)>,
    /// Bytes in the driver's buffers (counted against the limit).
    used: u64,
    /// The driver's live buffers: IOVA -> length.
    driver: BTreeMap<u64, u64>,
}

/// Who a buffer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Driver,
    Shim,
}

/// Allocates DMA buffers for one device, up to a total limit.
pub struct DmaAllocator {
    domain: Arc<Domain>,
    state: Arc<Mutex<State>>,
    limit: u64,
}

impl DmaAllocator {
    pub(crate) fn new(domain: Arc<Domain>, limit: u64, regions: Regions) -> Self {
        DmaAllocator {
            domain,
            state: Arc::new(Mutex::new(State {
                high: regions.high,
                low: regions.low,
                used: 0,
                driver: BTreeMap::new(),
            })),
            limit,
        }
    }

    /// Bytes currently allocated to the driver.
    pub fn used(&self) -> u64 {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).used
    }

    /// Allocate a zeroed buffer for the driver, of at least `size` bytes,
    /// rounded up to whole pages.
    pub fn allocate(&self, size: u32) -> Result<DmaBuffer, DmaError> {
        self.allocate_for(size, Owner::Driver, Region::High)
    }

    /// Like [`DmaAllocator::allocate`], but the whole buffer lies below
    /// 4 GiB, for devices that can only address 32 bits.
    pub fn allocate32(&self, size: u32) -> Result<DmaBuffer, DmaError> {
        self.allocate_for(size, Owner::Driver, Region::Low)
    }

    /// Allocate a zeroed buffer the shim keeps for itself. It is not the
    /// driver's, so [`DmaAllocator::driver_owns`] never covers it, and it
    /// does not count against the driver's limit.
    pub(crate) fn allocate_shim(&self, size: u32) -> Result<DmaBuffer, DmaError> {
        self.allocate_for(size, Owner::Shim, Region::High)
    }

    /// Whether `[iova, iova + len)` lies entirely inside one of the
    /// driver's live buffers. False for `len == 0`.
    pub fn driver_owns(&self, iova: u64, len: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        covers(&state.driver, iova, len)
    }

    fn allocate_for(&self, size: u32, owner: Owner, region: Region) -> Result<DmaBuffer, DmaError> {
        if size == 0 || size > MAX_BUFFER {
            return Err(DmaError::BadSize);
        }
        let len = u64::from(size).div_ceil(PAGE) * PAGE;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some((iova, end)) = (match region {
            Region::High => state.high,
            Region::Low => state.low,
        }) else {
            emit(
                "dma-error",
                &[
                    ("attempted", Value::Str("allocate below 4 GiB")),
                    (
                        "expected",
                        Value::Str("a low IOVA region (usable IOVAs from 1 MiB)"),
                    ),
                    (
                        "got",
                        Value::Str("none: the IOMMU reported no usable range there"),
                    ),
                ],
            );
            return Err(DmaError::OutOfMemory);
        };
        let over_limit = owner == Owner::Driver && state.used + len > self.limit;
        if over_limit || iova + len > end {
            return Err(DmaError::OutOfMemory);
        }

        let pages = (len / PAGE) as usize;
        let mem: Box<[Page]> = (0..pages).map(|_| Page([0; 4096])).collect();
        let raw: *mut [Page] = Box::into_raw(mem);
        let ptr = NonNull::new(raw as *mut u8).expect("Box::into_raw is never null");

        let mut map = DmaMap {
            argsz: argsz::<DmaMap>(),
            flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
            vaddr: ptr.as_ptr() as u64,
            iova,
            size: len,
        };
        if let Err(e) = ioctl_ptr(&self.domain.container, VFIO_IOMMU_MAP_DMA, &mut map) {
            // SAFETY: `raw` came from Box::into_raw above and was never mapped.
            drop(unsafe { Box::from_raw(raw) });
            emit(
                "dma-map-error",
                &[
                    ("attempted", Value::Str("VFIO_IOMMU_MAP_DMA")),
                    ("iova", Value::Hex(iova)),
                    ("size", Value::Hex(len)),
                    ("error", Value::Str(&e.to_string())),
                ],
            );
            return Err(DmaError::Io(e));
        }

        // Leave an unmapped guard page.
        let next = Some((iova + len + PAGE, end));
        match region {
            Region::High => state.high = next,
            Region::Low => state.low = next,
        }
        if owner == Owner::Driver {
            state.used += len;
            state.driver.insert(iova, len);
        }
        Ok(DmaBuffer {
            domain: Arc::clone(&self.domain),
            state: Arc::clone(&self.state),
            ptr,
            pages,
            iova,
            len,
            owner,
        })
    }
}

/// One mapped buffer. Dropping it unmaps it, then frees the memory.
pub struct DmaBuffer {
    domain: Arc<Domain>,
    state: Arc<Mutex<State>>,
    ptr: NonNull<u8>,
    pages: usize,
    iova: u64,
    len: u64,
    owner: Owner,
}

/// Whether `[iova, iova + len)` lies inside one range of `ranges`.
fn covers(ranges: &BTreeMap<u64, u64>, iova: u64, len: u64) -> bool {
    let Some(end) = iova.checked_add(len) else {
        return false;
    };
    len > 0
        && ranges
            .range(..=iova)
            .next_back()
            .is_some_and(|(&start, &size)| end <= start + size)
}

// SAFETY: the buffer exclusively owns its allocation (no references to it
// exist outside this struct), so moving it to another thread is sound.
unsafe impl Send for DmaBuffer {}

impl DmaBuffer {
    pub fn iova(&self) -> u64 {
        self.iova
    }

    pub fn size(&self) -> u32 {
        self.len as u32
    }

    fn check(&self, offset: u32, len: usize) -> Result<usize, DmaError> {
        if u64::from(offset) + len as u64 > self.len {
            return Err(DmaError::OutOfRange);
        }
        Ok(offset as usize)
    }

    pub fn read(&self, offset: u32, len: u32) -> Result<Vec<u8>, DmaError> {
        let start = self.check(offset, len as usize)?;
        fence(Ordering::SeqCst);
        let mut out = vec![0u8; len as usize];
        // SAFETY: `check` keeps start..start + len inside the allocation we
        // own. Volatile because the device may write this memory at any time.
        unsafe { volatile_read(self.ptr.as_ptr().add(start), &mut out) };
        Ok(out)
    }

    pub fn write(&self, offset: u32, data: &[u8]) -> Result<(), DmaError> {
        let start = self.check(offset, data.len())?;
        // SAFETY: as in `read`.
        unsafe { volatile_write(self.ptr.as_ptr().add(start), data) };
        fence(Ordering::SeqCst);
        Ok(())
    }
}

/// Copy `out.len()` bytes from `src` with volatile reads (issue #11): single
/// bytes until `src` is 8-byte aligned, then aligned u64 reads, then the
/// remaining bytes. The same bytes as one volatile read per byte, in about
/// an eighth of the accesses.
///
/// # Safety
/// `src..src + out.len()` must be valid for reads.
unsafe fn volatile_read(src: *const u8, out: &mut [u8]) {
    let n = out.len();
    let head = src.align_offset(8).min(n);
    let mut i = 0;
    while i < head {
        out[i] = src.add(i).read_volatile();
        i += 1;
    }
    while i + 8 <= n {
        let w = (src.add(i) as *const u64).read_volatile();
        out[i..i + 8].copy_from_slice(&w.to_ne_bytes());
        i += 8;
    }
    while i < n {
        out[i] = src.add(i).read_volatile();
        i += 1;
    }
}

/// Copy `data` to `dst` with volatile writes, aligned as in
/// [`volatile_read`].
///
/// # Safety
/// `dst..dst + data.len()` must be valid for writes.
unsafe fn volatile_write(dst: *mut u8, data: &[u8]) {
    let n = data.len();
    let head = dst.align_offset(8).min(n);
    let mut i = 0;
    while i < head {
        dst.add(i).write_volatile(data[i]);
        i += 1;
    }
    while i + 8 <= n {
        let w = u64::from_ne_bytes(data[i..i + 8].try_into().expect("8 bytes"));
        (dst.add(i) as *mut u64).write_volatile(w);
        i += 8;
    }
    while i < n {
        dst.add(i).write_volatile(data[i]);
        i += 1;
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        // From here on the driver may not name this range in a command,
        // whether or not the unmap below succeeds.
        if self.owner == Owner::Driver {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.driver.remove(&self.iova);
        }
        let mut unmap = DmaUnmap {
            argsz: argsz::<DmaUnmap>(),
            flags: 0,
            iova: self.iova,
            size: self.len,
        };
        let result = ioctl_ptr(&self.domain.container, VFIO_IOMMU_UNMAP_DMA, &mut unmap);
        match result {
            Ok(_) if unmap.size == self.len => {
                let raw =
                    std::ptr::slice_from_raw_parts_mut(self.ptr.as_ptr() as *mut Page, self.pages);
                // SAFETY: `raw` is the Box allocation from `allocate`, and it
                // is no longer mapped, so the device cannot reach it.
                drop(unsafe { Box::from_raw(raw) });
                if self.owner == Owner::Driver {
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.used -= self.len;
                }
            }
            other => {
                // The device might still reach this memory, so it is leaked
                // rather than freed.
                let got = match other {
                    Ok(_) => format!("unmapped 0x{:x} bytes", unmap.size),
                    Err(e) => e.to_string(),
                };
                emit(
                    "dma-unmap-error",
                    &[
                        ("attempted", Value::Str("VFIO_IOMMU_UNMAP_DMA")),
                        ("iova", Value::Hex(self.iova)),
                        (
                            "expected",
                            Value::Str(&format!("unmapped 0x{:x} bytes", self.len)),
                        ),
                        ("got", Value::Str(&got)),
                        ("action", Value::Str("memory leaked, not freed")),
                    ],
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn volatile_copies_match_byte_copies_at_every_offset_and_length() {
        // Two 8-byte-aligned pages of distinct bytes.
        let mut backing = vec![0u64; 1024];
        let base = backing.as_mut_ptr() as *mut u8;
        let bytes = 8192;
        let reference: Vec<u8> = (0..bytes).map(|i| (i * 7 + i / 251) as u8).collect();
        unsafe { std::ptr::copy_nonoverlapping(reference.as_ptr(), base, bytes) };
        for offset in (0..24).chain([4093, 4096, 4100]) {
            for len in (0..40).chain([4095, 4096, 4097]) {
                if offset + len > bytes {
                    continue;
                }
                let mut out = vec![0xaa; len];
                unsafe { super::volatile_read(base.add(offset), &mut out) };
                assert_eq!(out, reference[offset..offset + len], "read {offset}+{len}");

                let data: Vec<u8> = (0..len).map(|i| (i as u8) ^ 0x5c).collect();
                unsafe { super::volatile_write(base.add(offset), &data) };
                let mut want = reference.clone();
                want[offset..offset + len].copy_from_slice(&data);
                let got = unsafe { std::slice::from_raw_parts(base, bytes) };
                assert_eq!(got, &want[..], "write {offset}+{len}");
                unsafe { std::ptr::copy_nonoverlapping(reference.as_ptr(), base, bytes) };
            }
        }
    }

    /// Timing, not correctness: `cargo test -p grind-shim --release --
    /// --ignored --nocapture volatile_copy_speed`.
    #[test]
    #[ignore]
    fn volatile_copy_speed() {
        let mut backing = vec![0u64; 512];
        let base = backing.as_mut_ptr() as *mut u8;
        let mut out = vec![0u8; 4096];
        let rounds = 20_000;
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            for (i, b) in out.iter_mut().enumerate() {
                *b = unsafe { base.add(i).read_volatile() };
            }
        }
        let bytewise = t.elapsed() / rounds;
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            unsafe { super::volatile_read(base, &mut out) };
        }
        let wordwise = t.elapsed() / rounds;
        println!("4 KiB read: byte at a time {bytewise:?}, 8 bytes at a time {wordwise:?}");
    }

    use super::*;

    /// What QEMU 6.2.0's Intel IOMMU reports for each test device.
    const QEMU: [(u64, u64); 2] = [(0x0, 0xfedf_ffff), (0xfef0_0000, 0x7f_ffff_ffff)];

    #[test]
    fn regions_from_the_measured_qemu_ranges() {
        let r = Regions::from_ranges(Some(&QEMU));
        assert_eq!(r.high, Some((1 << 32, 1 << 39)));
        assert_eq!(
            r.low,
            Some((1 << 20, 0xfee0_0000)),
            "stops at the MSI window"
        );
    }

    #[test]
    fn regions_without_reported_ranges() {
        let r = Regions::from_ranges(None);
        assert_eq!(r.high, Some((1 << 32, 1 << 39)), "as before");
        assert_eq!(r.low, None, "no low region without knowing the holes");
    }

    #[test]
    fn low_region_stops_at_the_first_hole_and_at_4_gib() {
        // A hole at 16 MiB: the low region is 1-16 MiB only.
        let r = Regions::from_ranges(Some(&[(0, 0xff_ffff), (0x200_0000, u64::MAX)]));
        assert_eq!(r.low, Some((1 << 20, 0x100_0000)));
        assert_eq!(r.high, Some((1 << 32, u64::MAX)));
        // One range covering everything: low still ends at 4 GiB.
        let r = Regions::from_ranges(Some(&[(0, u64::MAX)]));
        assert_eq!(r.low, Some((1 << 20, 1 << 32)));
        // Nothing usable at 1 MiB: no low region.
        let r = Regions::from_ranges(Some(&[(0x8000_0000, u64::MAX)]));
        assert_eq!(r.low, None);
        // A range ending below 4 GiB and nothing above: no high region.
        let r = Regions::from_ranges(Some(&[(0, 0xfedf_ffff)]));
        assert_eq!(r.high, None);
        assert_eq!(r.low, Some((1 << 20, 0xfee0_0000)));
    }

    /// A GET_INFO reply: flags with CAPS, a migration capability (id 2)
    /// first, then the IOVA-range capability with `ranges`.
    fn reply(ranges: &[(u64, u64)]) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[4..8].copy_from_slice(&3u32.to_le_bytes()); // PGSIZES | CAPS
        b[16..20].copy_from_slice(&24u32.to_le_bytes()); // first cap at 24
        b[24..26].copy_from_slice(&2u16.to_le_bytes()); // id 2
        b[28..32].copy_from_slice(&48u32.to_le_bytes()); // next at 48
        b[48..50].copy_from_slice(&1u16.to_le_bytes()); // IOVA range
        b[56..60].copy_from_slice(&(ranges.len() as u32).to_le_bytes());
        for (i, (start, end)) in ranges.iter().enumerate() {
            b[64 + 16 * i..72 + 16 * i].copy_from_slice(&start.to_le_bytes());
            b[72 + 16 * i..80 + 16 * i].copy_from_slice(&end.to_le_bytes());
        }
        b
    }

    #[test]
    fn parse_iova_ranges_follows_the_capability_chain() {
        assert_eq!(parse_iova_ranges(&reply(&QEMU)), Some(QEMU.to_vec()));
        let mut no_caps = reply(&QEMU);
        no_caps[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(parse_iova_ranges(&no_caps), None);
        let mut no_range_cap = reply(&QEMU);
        no_range_cap[48..50].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(parse_iova_ranges(&no_range_cap), None);
        let mut looped = reply(&QEMU);
        looped[48..50].copy_from_slice(&3u16.to_le_bytes());
        looped[52..56].copy_from_slice(&24u32.to_le_bytes()); // back to the first
        assert_eq!(parse_iova_ranges(&looped), None, "a loop ends");
        let mut too_many = reply(&QEMU);
        too_many[56..60].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert_eq!(parse_iova_ranges(&too_many), None, "ranges past the end");
    }

    #[test]
    fn covers_only_whole_ranges_inside_one_buffer() {
        let mut r = BTreeMap::new();
        r.insert(0x1_0000_0000, 0x2000); // two pages
        r.insert(0x1_0000_3000, 0x1000); // after a guard page
        let base = 0x1_0000_0000u64;
        assert!(covers(&r, base, 1));
        assert!(covers(&r, base, 0x2000));
        assert!(covers(&r, base + 0x1ffc, 4));
        assert!(!covers(&r, base + 0x1ffc, 8), "runs into the guard page");
        assert!(!covers(&r, base, 0x4000), "spans two buffers and the gap");
        assert!(!covers(&r, base - 4, 8), "starts before the buffer");
        assert!(!covers(&r, base + 0x2000, 4), "the guard page");
        assert!(covers(&r, base + 0x3000, 0x1000));
        assert!(!covers(&r, base, 0), "empty ranges are never covered");
        assert!(!covers(&r, u64::MAX - 1, 4), "overflow");
        assert!(!covers(&BTreeMap::new(), base, 4));
    }
}
