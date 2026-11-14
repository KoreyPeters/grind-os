// SPDX-License-Identifier: AGPL-3.0-or-later

//! Negative control: a "block driver" that never touches the device but
//! knows the test. It synthesizes the seed pattern for reads, keeps writes
//! in memory, and follows the block error contract exactly, so every check
//! grind-host makes in the VM passes. Only the image check after the VM
//! powers off can catch it: the disk still holds the seed where the data
//! pattern should be.

const LENIENT_READ: bool = false;
const PARTIAL_WRITE: bool = false;

include!("../../cheat-common/cheat.rs");
