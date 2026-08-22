// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Operator/admin writes: tune the alpha policy parameter.

use rusqlite::{Result, params};

use super::Storage;

impl Storage {
    /// Set the alpha knob from a `num/denom` rational. Computes both
    /// `log_alpha` and `log_one_plus_alpha` (the policy-derived view needs
    /// both). Panics if `num + denom` overflows `u64` — a misuse, not a
    /// runtime condition.
    pub fn set_alpha(&mut self, num: u64, denom: u64) -> Result<()> {
        use sequencer_core::fee::log_fee_ratio;

        let log_alpha = log_fee_ratio(num, denom);
        let one_plus_alpha_num = num.checked_add(denom).expect(
            "set_alpha: num + denom overflows u64; use smaller values for the alpha fraction",
        );
        let log_one_plus_alpha = log_fee_ratio(one_plus_alpha_num, denom);

        let changed = self.conn.execute(
            "UPDATE batch_policy \
             SET log_alpha = ?1, log_one_plus_alpha = ?2 \
             WHERE singleton_id = 0",
            params![i64::from(log_alpha), i64::from(log_one_plus_alpha)],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::{Storage, test_helpers::temp_db};

    #[test]
    fn negative_batch_size_target_floors_to_zero() {
        let db = temp_db("floor-batch-target");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        // This row satisfies every schema constraint while deriving a negative
        // target: 1403 - 2000 - 419 = -1016.
        storage
            .conn
            .execute(
                "UPDATE batch_policy SET log_alpha = 2000 WHERE singleton_id = 0",
                [],
            )
            .expect("set high alpha");

        let policy = storage.batch_policy().expect("read policy");
        assert_eq!(
            policy.batch_size_target, 0,
            "negative batch-size target should be floored to zero"
        );
    }

    #[test]
    #[should_panic(expected = "batch policy derived recommended fee")]
    fn negative_recommended_fee_fails_loud() {
        let db = temp_db("negative-recommended-fee");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        // A negative derived fee is ruled out by the column constraints. Model
        // corruption/tampering that bypassed them and ensure the read cannot
        // launder the impossible value into a plausible zero fee.
        storage
            .conn
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE batch_policy SET log_delta = -10000 WHERE singleton_id = 0;",
            )
            .expect("inject impossible policy row");

        let _ = storage.batch_policy();
    }

    #[test]
    #[should_panic(expected = "batch policy derived batch size target")]
    fn oversized_batch_size_target_fails_loud() {
        let db = temp_db("oversized-batch-target");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        // This is schema-valid: the derived target is 17984, which remains
        // below the row's configured log_max_batch_bytes of 20000. It is still
        // outside the core fee exponent domain and must not become u64::MAX.
        storage
            .conn
            .execute(
                "UPDATE batch_policy
                 SET log_alpha = -17000, log_max_batch_bytes = 20000
                 WHERE singleton_id = 0",
                [],
            )
            .expect("set schema-valid oversized target");
        let integrity: String = storage
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .expect("check database integrity");
        assert_eq!(integrity, "ok");

        let _ = storage.batch_policy();
    }

    #[test]
    #[should_panic(expected = "num + denom overflows u64")]
    fn set_alpha_rejects_overflow() {
        let db = temp_db("alpha-overflow");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage.set_alpha(u64::MAX, 1).unwrap();
    }

    /// CHECK constraint guards against alpha values that would push the batch-size
    /// target past `log_max_batch_bytes`. Migrated from the old `sql.rs` test suite.
    #[test]
    fn batch_policy_check_rejects_unsafe_alpha() {
        let db = temp_db("unsafe-alpha");
        let storage = Storage::open(db.path.as_str()).expect("open storage");
        // log_alpha=-350 → log_batch_size_target = 1403-(-350)-419 = 1334 >= log_max_batch_bytes=1333
        let err = storage.conn.execute(
            "UPDATE batch_policy SET log_alpha = ?1, log_one_plus_alpha = ?2 WHERE singleton_id = 0",
            [-350_i64, 0_i64],
        );
        assert!(
            err.is_err(),
            "CHECK should reject unsafe alpha (log_batch_size_target >= log_max_batch_bytes)"
        );
    }
}
