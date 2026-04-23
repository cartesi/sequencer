// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Saturating width conversions between Rust and SQLite integer types, plus
//! `SystemTime` ↔ `i64` Unix-ms conversions.
//!
//! SQLite stores integers as `INTEGER` (signed 64-bit). Rust domain types use
//! narrower unsigned widths (`u16`, `u32`, `u64`). The conversions here are
//! load-bearing glue that the rest of the storage module calls pervasively.
//!
//! All conversions saturate rather than panic — the domain values we persist
//! are always non-negative and well within `i64::MAX`, but saturation keeps
//! corrupted or malicious DB rows from crashing the process.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ── Time helpers ──────────────────────────────────────────────────────────

pub(super) fn to_unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub(super) fn from_unix_ms(ms: i64) -> SystemTime {
    let clamped_ms = ms.max(0) as u64;
    UNIX_EPOCH + Duration::from_millis(clamped_ms)
}

/// Current wall-clock time as an `i64` SQLite timestamp.
///
/// Delegates to [`crate::runtime::clock::unix_now_ms`] so the whole crate goes
/// through one clock entry point.
pub(super) fn now_unix_ms() -> i64 {
    i64::try_from(crate::runtime::clock::unix_now_ms()).unwrap_or(i64::MAX)
}

// ── Width conversions ─────────────────────────────────────────────────────

pub(super) fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn i64_to_u64(value: i64) -> u64 {
    value.max(0) as u64
}

pub(super) fn i64_to_u16(value: i64) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

pub(super) fn i64_to_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}
