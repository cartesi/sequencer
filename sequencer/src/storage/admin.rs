// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Operator/admin writes: tune fee policy parameters (`set_log_gas_price`,
//! `set_alpha`). Used today by tests and ad-hoc operator commands; not on the
//! hot path.

use rusqlite::{Result, params};

use super::Storage;

impl Storage {
    pub fn set_log_gas_price(&mut self, log_gas_price: u16) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE batch_policy SET log_gas_price = ?1 WHERE singleton_id = 0",
            params![i64::from(log_gas_price)],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }
        Ok(())
    }

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
    fn high_gas_price_clamps_recommended_fee_to_max_exponent() {
        let db = temp_db("clamp-fee");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Set gas price high enough that log_recommended_fee > MAX_EXPONENT (17101).
        // Default: log_recommended_fee = gas_price + 20 + 419 + 621.
        // With gas_price = 17000: 17000 + 1060 = 18060 > 17101.
        storage
            .set_log_gas_price(17000)
            .expect("set high gas price");

        let policy = storage.batch_policy().expect("read policy");
        assert_eq!(
            policy.recommended_fee,
            sequencer_core::fee::MAX_EXPONENT,
            "recommended_fee should be clamped to MAX_EXPONENT"
        );

        // fee_to_linear must not panic with the clamped value.
        let _ = sequencer_core::fee::fee_to_linear(policy.recommended_fee);
    }

    #[test]
    #[should_panic(expected = "num + denom overflows u64")]
    fn set_alpha_rejects_overflow() {
        let db = temp_db("alpha-overflow");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        storage.set_alpha(u64::MAX, 1).unwrap();
    }

    /// CHECK constraint guards against alpha values that would push the batch-size
    /// target past `log_max_batch_bytes`. Migrated from the old `sql.rs` test suite.
    #[test]
    fn batch_policy_check_rejects_unsafe_alpha() {
        let db = temp_db("unsafe-alpha");
        let storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
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
