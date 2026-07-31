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
