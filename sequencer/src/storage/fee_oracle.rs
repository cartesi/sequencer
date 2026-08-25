// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Fee-oracle writes to the batch policy.

use rusqlite::{Result, params};

use super::Storage;
use super::convert::{i64_to_u64, now_unix_ms};

impl Storage {
    pub fn log_gas_price(&self) -> Result<u16> {
        self.conn.query_row(
            "SELECT log_gas_price FROM batch_policy WHERE singleton_id = 0",
            [],
            |row| row.get::<_, u16>(0),
        )
    }

    /// Unix-ms of the last successful `log_gas_price` write. `0` means never
    /// written (migration default); completed setup guarantees a nonzero value.
    pub fn log_gas_price_updated_at_ms(&self) -> Result<u64> {
        self.conn.query_row(
            "SELECT log_gas_price_updated_at_ms FROM batch_policy WHERE singleton_id = 0",
            [],
            |row| Ok(i64_to_u64(row.get::<_, i64>(0)?)),
        )
    }

    pub fn set_log_gas_price(&mut self, log_gas_price: u16) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE batch_policy \
             SET log_gas_price = ?1, log_gas_price_updated_at_ms = ?2 \
             WHERE singleton_id = 0",
            params![i64::from(log_gas_price), now_unix_ms()],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }
        Ok(())
    }

    /// Test-only: pin the observation stamp without going through wall-clock now.
    #[cfg(test)]
    pub fn set_log_gas_price_updated_at_ms_for_test(&mut self, updated_at_ms: u64) -> Result<()> {
        use super::convert::u64_to_i64;
        let changed = self.conn.execute(
            "UPDATE batch_policy SET log_gas_price_updated_at_ms = ?1 WHERE singleton_id = 0",
            params![u64_to_i64(updated_at_ms)],
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
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .set_log_gas_price(17000)
            .expect("set high gas price");

        let policy = storage.batch_policy().expect("read policy");
        assert_eq!(policy.recommended_fee, sequencer_core::fee::MAX_EXPONENT);
        let _ = sequencer_core::fee::fee_to_linear(policy.recommended_fee);
    }

    #[test]
    fn set_log_gas_price_stamps_updated_at() {
        let db = temp_db("fee-price-stamp");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        assert_eq!(storage.log_gas_price_updated_at_ms().unwrap(), 0);

        storage.set_log_gas_price(42).expect("set");
        let updated_at = storage.log_gas_price_updated_at_ms().unwrap();
        assert!(updated_at > 0);
    }
}
