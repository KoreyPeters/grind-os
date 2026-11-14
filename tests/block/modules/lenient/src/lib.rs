// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control: the cheat, except that reads outside the disk return
//! data instead of an error. grind-host must fail it in the VM, at the
//! first out-of-range read (block-accepted-invalid).

const LENIENT_READ: bool = true;
const PARTIAL_WRITE: bool = false;

include!("../../cheat-common/cheat.rs");
