// SPDX-License-Identifier: AGPL-3.0-or-later

//! Grind OS bus shim: the only code that touches a device for a driver.
//!
//! The shim opens one PCI device through VFIO and exposes exactly two things:
//!
//! - [`Window`]: 32-bit reads and writes to BAR0, bounds- and
//!   alignment-checked, with writes filtered through the device's
//!   [`deny::WriteList`], a deny list or an allow list (rule 10).
//! - [`DmaAllocator`]: zeroed buffers pinned and mapped into the device's
//!   IOMMU domain, addressed by the driver only through their IOVA.
//!
//! Everything else (config space, other BARs, the container and group) stays
//! inside the shim. Every refusal and unexpected failure is logged as a JSON
//! line (see [`log`]).

pub mod deny;
mod device;
mod dma;
pub mod log;
mod sys;
mod window;

pub use device::{Device, OpenError};
pub use dma::{DmaAllocator, DmaBuffer, DmaError};
pub use window::{MmioError, RegionIo, Window};
