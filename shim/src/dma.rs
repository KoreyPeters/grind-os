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
/// IOVAs start above 4 GiB, clear of the x86 MSI window at 0xfee00000.
const IOVA_BASE: u64 = 1 << 32;
/// QEMU's emulated Intel IOMMU has a 39-bit address width.
const IOVA_LIMIT: u64 = 1 << 39;

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
    /// The allocator's total limit (or the IOVA space) is used up.
    OutOfMemory,
    /// The access does not fit inside the buffer.
    OutOfRange,
    /// Mapping failed. Not the driver's fault; already logged.
    Io(io::Error),
}

struct State {
    next_iova: u64,
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
    pub(crate) fn new(domain: Arc<Domain>, limit: u64) -> Self {
        DmaAllocator {
            domain,
            state: Arc::new(Mutex::new(State {
                next_iova: IOVA_BASE,
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
        self.allocate_for(size, Owner::Driver)
    }

    /// Allocate a zeroed buffer the shim keeps for itself. It is not the
    /// driver's, so [`DmaAllocator::driver_owns`] never covers it, and it
    /// does not count against the driver's limit.
    pub(crate) fn allocate_shim(&self, size: u32) -> Result<DmaBuffer, DmaError> {
        self.allocate_for(size, Owner::Shim)
    }

    /// Whether `[iova, iova + len)` lies entirely inside one of the
    /// driver's live buffers. False for `len == 0`.
    pub fn driver_owns(&self, iova: u64, len: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        covers(&state.driver, iova, len)
    }

    fn allocate_for(&self, size: u32, owner: Owner) -> Result<DmaBuffer, DmaError> {
        if size == 0 || size > MAX_BUFFER {
            return Err(DmaError::BadSize);
        }
        let len = u64::from(size).div_ceil(PAGE) * PAGE;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let iova = state.next_iova;
        let over_limit = owner == Owner::Driver && state.used + len > self.limit;
        if over_limit || iova + len > IOVA_LIMIT {
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

        state.next_iova = iova + len + PAGE; // leave an unmapped guard page
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
        let base = self.ptr.as_ptr();
        // SAFETY: start + i < self.len, inside the allocation we own. Volatile
        // because the device may write this memory at any time.
        Ok((0..len as usize)
            .map(|i| unsafe { base.add(start + i).read_volatile() })
            .collect())
    }

    pub fn write(&self, offset: u32, data: &[u8]) -> Result<(), DmaError> {
        let start = self.check(offset, data.len())?;
        let base = self.ptr.as_ptr();
        for (i, b) in data.iter().enumerate() {
            // SAFETY: as in `read`.
            unsafe { base.add(start + i).write_volatile(*b) };
        }
        fence(Ordering::SeqCst);
        Ok(())
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
    use super::*;

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
