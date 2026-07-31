// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Fee-oracle writes to the batch policy.

use rusqlite::{Result, params};

use super::Storage;

impl Storage {
    pub fn log_gas_price(&self) -> Result<u16> {
        self.conn.query_row(
            "SELECT log_gas_price FROM batch_policy WHERE singleton_id = 0",
            [],
            |row| row.get::<_, u16>(0),
        )
    }

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
}
