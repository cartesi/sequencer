// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Integer conversions between Rust domain types and SQLite `INTEGER` columns,
//! plus `SystemTime` ↔ `i64` Unix-ms conversions.
//!
//! Two conversion families with opposite postures, per the fail-loud check
//! policy in `docs/invariants.md`:
//!
//! - **Contract-bound conversions fail loud.** Domain values (indices, nonces,
//!   block numbers, offsets, fees) are non-negative and far below `i64::MAX`
//!   by schema `CHECK`s, triggers, or writer-side types. An out-of-range value
//!   can only mean DB corruption, tampering, or a sequencer bug; saturating it
//!   would fabricate a plausible value and let the divergence externalize (a
//!   signed batch, a feed event, a wrong recovery pivot). These panic to stop
//!   the operation before externalization. A persistent violation is not
//!   repaired by restart and may require inspection or cockroach recovery;
//!   fail-loud is a safety property, not a self-healing claim.
//! - **Clock and query-bound conversions saturate.** Wall-clock time is
//!   environmental, not an invariant (review F8): a far-future clock clamps to
//!   `i64::MAX` rather than aborting. Untrusted or config-sourced SQL bounds
//!   go through [`saturating_query_bound`], where clamping preserves the
//!   comparison semantics exactly. Sign remains contract-bound even for clock
//!   *columns*: every timestamp writer is u64-clock-sourced (floored at 0), so
//!   [`from_unix_ms`] fail-louds on a negative stored value — a sign check is
//!   a real invariant, unlike the ordering checks F8 warns about.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
#[error("{field} value {value} exceeds SQLite INTEGER maximum")]
struct ExternalIntegerRangeError {
    field: &'static str,
    value: u64,
}

// ── Time helpers ──────────────────────────────────────────────────────────

/// Saturating: a pre-epoch clock clamps to 0, a far-future clock to
/// `i64::MAX`. Wall-clock state is environmental (F8), never an invariant.
pub(super) fn to_unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

/// Fail-loud on sign: every timestamp writer is u64-clock-sourced, so a
/// negative stored Unix-ms value is contract-impossible. Ordering and
/// monotonicity are deliberately NOT checked here (review F8: wall-clock
/// regression is legitimate).
pub(super) fn from_unix_ms(ms: i64) -> SystemTime {
    let ms = u64::try_from(ms).unwrap_or_else(|_| {
        panic!("stored unix-ms timestamp {ms} is negative: contract-impossible")
    });
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// Current wall-clock time as an `i64` SQLite timestamp. Saturating (see
/// [`to_unix_ms`]).
///
/// Delegates to [`crate::clock::unix_now_ms`] so the whole crate goes
/// through one clock entry point.
pub(super) fn now_unix_ms() -> i64 {
    i64::try_from(crate::clock::unix_now_ms()).unwrap_or(i64::MAX)
}

// ── Contract-bound width conversions (fail loud) ──────────────────────────

pub(super) fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value)
        .unwrap_or_else(|_| panic!("domain value {value} exceeds i64::MAX: contract-impossible"))
}

/// Checked conversion for configuration/provider values crossing into the
/// SQLite representation. An unrepresentable external value is a typed
/// boundary refusal, not an internal invariant panic.
pub(super) fn external_u64_to_i64(value: u64, field: &'static str) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|_| {
        rusqlite::Error::ToSqlConversionFailure(Box::new(ExternalIntegerRangeError {
            field,
            value,
        }))
    })
}

pub(super) fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value)
        .unwrap_or_else(|_| panic!("stored value {value} is negative: contract-impossible"))
}

pub(super) fn i64_to_u16(value: i64) -> u16 {
    u16::try_from(value).unwrap_or_else(|_| {
        panic!("stored value {value} is outside u16 range: contract-impossible")
    })
}

pub(super) fn i64_to_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or_else(|_| {
        panic!("stored value {value} is outside u32 range: contract-impossible")
    })
}

// ── Query-bound conversion (saturating, by design) ────────────────────────

/// Saturating clamp for **untrusted or config-sourced** SQL query bounds:
/// WS `from_offset` cursors, page/count `LIMIT`s, and setup/recovery block
/// predicates. The full `u64` range is legal input here, and clamping to
/// `i64::MAX` preserves the comparison exactly — no SQLite `INTEGER` or rowid
/// exceeds `i64::MAX`, so a past-the-end lower bound matches zero rows while a
/// clamped upper bound or `LIMIT` includes every representable row.
///
/// Never use this for domain values read from or written to columns; those
/// go through the fail-loud converters above.
pub(super) fn saturating_query_bound(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Whether a SQLite error proves durable row/schema corruption or a
/// trusted-code contract violation, as opposed to an operational condition
/// such as BUSY, I/O failure, permissions, or disk pressure.
///
/// Egress uses this distinction to take the whole runtime offline on
/// persistent corruption without terminalizing transient database failures.
///
/// The trailing wildcard is forced (`rusqlite::Error` is non-exhaustive) and
/// deliberately fail-open toward *operational*: an unknown/new variant
/// restarts rather than terminalizes, because a wrong "terminal" pages an
/// operator for a self-healing condition while a wrong "operational" merely
/// retries into the same error until it is classified. Review the list on
/// every rusqlite upgrade.
pub(crate) fn is_persistent_storage_error(error: &rusqlite::Error) -> bool {
    use rusqlite::Error;
    use rusqlite::ffi::ErrorCode;

    match error {
        Error::SqliteFailure(source, _) => matches!(
            source.code,
            ErrorCode::InternalMalfunction
                | ErrorCode::DatabaseCorrupt
                | ErrorCode::SchemaChanged
                | ErrorCode::ConstraintViolation
                | ErrorCode::TypeMismatch
                | ErrorCode::ApiMisuse
                | ErrorCode::NotADatabase
                | ErrorCode::Unknown
        ),
        Error::FromSqlConversionFailure(..)
        | Error::IntegralValueOutOfRange(..)
        | Error::Utf8Error(..)
        | Error::NulError(..)
        | Error::InvalidParameterName(..)
        | Error::ExecuteReturnedResults
        | Error::QueryReturnedNoRows
        | Error::QueryReturnedMoreThanOneRow
        | Error::InvalidColumnIndex(..)
        | Error::InvalidColumnName(..)
        | Error::InvalidColumnType(..)
        | Error::StatementChangedRows(..)
        | Error::ToSqlConversionFailure(..)
        | Error::InvalidQuery
        | Error::UnwindingPanic
        | Error::MultipleStatement
        | Error::InvalidParameterCount(..) => true,
        // The modern_sqlite (bundled) build reports offset-bearing
        // prepare-time failures (malformed SQL) as SqlInputError; the same
        // trusted-SQL-broke condition without an offset ("no such table")
        // arrives as SqliteFailure(Unknown). Both spellings must classify
        // identically.
        Error::SqlInputError { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        external_u64_to_i64, from_unix_ms, i64_to_u16, i64_to_u32, i64_to_u64,
        is_persistent_storage_error, saturating_query_bound, to_unix_ms, u64_to_i64,
    };
    use std::time::{Duration, UNIX_EPOCH};

    const I64_MAX_U64: u64 = i64::MAX as u64;

    #[test]
    fn prepare_time_sql_failures_classify_persistent_in_both_spellings() {
        // Trusted SQL breaking at prepare time has two rusqlite spellings in
        // the bundled (modern_sqlite) build: offset-bearing malformed SQL is
        // SqlInputError, while "no such table" (no offset) stays
        // SqliteFailure(Unknown). Both must classify persistent.
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");

        let err = conn
            .prepare("SELECT 1 FRO somewhere")
            .expect_err("prepare of malformed SQL must fail");
        assert!(
            matches!(err, rusqlite::Error::SqlInputError { .. }),
            "expected SqlInputError from an offset-bearing prepare failure, got {err:?}"
        );
        assert!(is_persistent_storage_error(&err));

        let err = conn
            .prepare("SELECT value FROM definitely_missing_table")
            .expect_err("prepare against a missing table must fail");
        assert!(
            matches!(
                &err,
                rusqlite::Error::SqliteFailure(source, _)
                    if source.code == rusqlite::ffi::ErrorCode::Unknown
            ),
            "expected SqliteFailure(Unknown) from a missing table, got {err:?}"
        );
        assert!(is_persistent_storage_error(&err));
    }

    #[test]
    fn unix_ms_conversion_saturates_environmental_time_boundaries() {
        assert_eq!(
            to_unix_ms(UNIX_EPOCH - Duration::from_millis(1)),
            0,
            "pre-epoch time floors at zero"
        );
        assert_eq!(to_unix_ms(UNIX_EPOCH), 0);
        assert_eq!(
            to_unix_ms(UNIX_EPOCH + Duration::from_millis(I64_MAX_U64)),
            i64::MAX
        );
        assert_eq!(
            to_unix_ms(UNIX_EPOCH + Duration::from_millis(I64_MAX_U64 + 1)),
            i64::MAX,
            "far-future time caps at SQLite's maximum INTEGER"
        );
    }

    #[test]
    fn stored_unix_ms_accepts_full_non_negative_i64_range() {
        assert_eq!(from_unix_ms(0), UNIX_EPOCH);
        assert_eq!(
            from_unix_ms(i64::MAX),
            UNIX_EPOCH + Duration::from_millis(I64_MAX_U64)
        );
    }

    #[test]
    #[should_panic(expected = "stored unix-ms timestamp -1 is negative: contract-impossible")]
    fn stored_unix_ms_rejects_negative_values() {
        let _ = from_unix_ms(-1);
    }

    #[test]
    fn u64_to_i64_accepts_representable_boundaries() {
        assert_eq!(u64_to_i64(0), 0);
        assert_eq!(u64_to_i64(I64_MAX_U64), i64::MAX);
    }

    #[test]
    #[should_panic(expected = "exceeds i64::MAX: contract-impossible")]
    fn u64_to_i64_rejects_first_unrepresentable_value() {
        let _ = u64_to_i64(I64_MAX_U64 + 1);
    }

    #[test]
    fn external_u64_to_i64_returns_a_typed_boundary_error() {
        assert_eq!(
            external_u64_to_i64(I64_MAX_U64, "test field").unwrap(),
            i64::MAX
        );
        let err = external_u64_to_i64(I64_MAX_U64 + 1, "test field")
            .expect_err("external value must be rejected");
        assert!(
            matches!(err, rusqlite::Error::ToSqlConversionFailure(_)),
            "unexpected error: {err}"
        );
        assert!(err.to_string().contains("test field"));
    }

    #[test]
    fn i64_to_u64_accepts_non_negative_boundaries() {
        assert_eq!(i64_to_u64(0), 0);
        assert_eq!(i64_to_u64(i64::MAX), I64_MAX_U64);
    }

    #[test]
    #[should_panic(expected = "stored value -1 is negative: contract-impossible")]
    fn i64_to_u64_rejects_negative_values() {
        let _ = i64_to_u64(-1);
    }

    #[test]
    fn i64_to_u16_accepts_unsigned_boundaries() {
        assert_eq!(i64_to_u16(0), 0);
        assert_eq!(i64_to_u16(i64::from(u16::MAX)), u16::MAX);
    }

    #[test]
    #[should_panic(expected = "outside u16 range: contract-impossible")]
    fn i64_to_u16_rejects_negative_values() {
        let _ = i64_to_u16(-1);
    }

    #[test]
    #[should_panic(expected = "outside u16 range: contract-impossible")]
    fn i64_to_u16_rejects_first_value_above_range() {
        let _ = i64_to_u16(i64::from(u16::MAX) + 1);
    }

    #[test]
    fn i64_to_u32_accepts_unsigned_boundaries() {
        assert_eq!(i64_to_u32(0), 0);
        assert_eq!(i64_to_u32(i64::from(u32::MAX)), u32::MAX);
    }

    #[test]
    #[should_panic(expected = "outside u32 range: contract-impossible")]
    fn i64_to_u32_rejects_negative_values() {
        let _ = i64_to_u32(-1);
    }

    #[test]
    #[should_panic(expected = "outside u32 range: contract-impossible")]
    fn i64_to_u32_rejects_first_value_above_range() {
        let _ = i64_to_u32(i64::from(u32::MAX) + 1);
    }

    #[test]
    fn query_bound_saturates_only_above_sqlite_integer_range() {
        assert_eq!(saturating_query_bound(0), 0);
        assert_eq!(saturating_query_bound(I64_MAX_U64), i64::MAX);
        assert_eq!(
            saturating_query_bound(I64_MAX_U64 + 1),
            i64::MAX,
            "first out-of-range bound caps"
        );
        assert_eq!(
            saturating_query_bound(u64::MAX),
            i64::MAX,
            "full input range is legal"
        );
    }

    #[test]
    fn persistent_storage_error_classifier_separates_corruption_from_io() {
        assert!(is_persistent_storage_error(
            &rusqlite::Error::QueryReturnedNoRows
        ));
        assert!(is_persistent_storage_error(
            &rusqlite::Error::InvalidColumnIndex(7)
        ));
        assert!(!is_persistent_storage_error(
            &rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                    extended_code: 5,
                },
                None,
            )
        ));
    }
}
