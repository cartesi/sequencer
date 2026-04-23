// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared clock helper.
//!
//! Every callsite that needs "now in Unix-ms" goes through [`unix_now_ms`] so
//! the sequencer has a single place to swap in a test clock if needed.
//! `SystemTime::now()` pre-epoch is defended against via `unwrap_or_default()`.

use std::time::SystemTime;

/// Current wall-clock time as Unix-ms. Passed into
/// [`crate::storage::Storage::check_danger`] and friends.
pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
