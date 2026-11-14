// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control: the cheat, except that a write running past the end
//! keeps the blocks that fit before returning an error. grind-host must
//! fail it in the VM: the last block reads back BADW, not DATA.

const LENIENT_READ: bool = false;
const PARTIAL_WRITE: bool = true;

include!("../../cheat-common/cheat.rs");
