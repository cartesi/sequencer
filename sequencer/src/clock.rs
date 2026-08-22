// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared clock helper.
//!
//! Every callsite that needs "now in Unix-ms" goes through [`unix_now_ms`] so
//! the sequencer has a single place to swap in a test clock if needed.
//! `SystemTime::now()` pre-epoch is defended against via `unwrap_or_default()`.

use std::time::{Duration, SystemTime};

/// Current wall-clock time as Unix-ms. Passed into
/// [`crate::storage::Storage::check_danger`] and friends.
pub fn unix_now_ms() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    duration_as_unix_ms(elapsed)
}

fn duration_as_unix_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_to_unix_ms_saturates_before_narrowing() {
        assert_eq!(duration_as_unix_ms(Duration::ZERO), 0);
        assert_eq!(
            duration_as_unix_ms(Duration::from_millis(u64::MAX)),
            u64::MAX
        );
        assert_eq!(
            duration_as_unix_ms(Duration::new(u64::MAX, 999_999_999)),
            u64::MAX,
            "far-future duration must not wrap to a plausible small clock"
        );
    }
}
