// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Input reader writer: ingests L1 InputBox events into `safe_inputs`,
//! advances `l1_safe_head`, and maintains the L1 bootstrap cache.
//!
//! Also exposes the read-side queries the input reader and other callers need
//! (current safe block, safe-input bounds, last-sync timestamp).

use alloy_primitives::Address;
use rusqlite::{OptionalExtension, Result, Transaction, TransactionBehavior, params};

use super::Storage;
use super::StoredSafeInput;
use super::internals::{
    i64_to_u64, now_unix_ms, query_current_safe_block, query_latest_safe_input_index_exclusive,
    u64_to_i64,
};

impl Storage {
    /// `MAX(safe_input_index) + 1` (or 0 if empty). The exclusive bound on the
    /// `safe_inputs` table — the next index a fresh row would receive.
    pub fn safe_input_end_exclusive(&mut self) -> Result<u64> {
        query_latest_safe_input_index_exclusive(&self.conn)
    }

    pub fn current_safe_block(&mut self) -> Result<u64> {
        query_current_safe_block(&self.conn)
    }

    /// Advance `l1_safe_head.block_number` to `minimum_safe_block` if it is
    /// behind. One-shot bootstrap helper — does NOT touch `synced_at_ms`, so
    /// it doesn't masquerade as a real L1 sync to the wall-clock danger
    /// estimator.
    pub fn ensure_minimum_safe_block(&mut self, minimum_safe_block: u64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = query_current_safe_block(&tx)?;
        if current < minimum_safe_block {
            // `synced_at_ms` is intentionally NOT touched here: this is a bootstrap
            // setup (genesis-block sync), not a real L1 read. Leaving it preserves
            // the wall-clock danger estimate's "time since last real sync" semantics.
            let changed = tx.execute(
                "UPDATE l1_safe_head SET block_number = ?1 WHERE singleton_id = 0",
                params![u64_to_i64(minimum_safe_block)],
            )?;
            if changed != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record that L1 was successfully queried at the current wall-clock time.
    pub fn touch_l1_sync(&mut self) -> Result<()> {
        let now_ms = now_unix_ms();
        let changed = self.conn.execute(
            "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
            params![now_ms],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }
        Ok(())
    }

    /// Atomically: insert `inputs` (assigned contiguous indexes starting from
    /// the current MAX+1), advance `l1_safe_head.block_number` to `safe_block`,
    /// and stamp `synced_at_ms`. Asserts `safe_block` is monotonic and that it
    /// strictly advances when `inputs` is non-empty.
    pub fn append_safe_inputs(
        &mut self,
        safe_block: u64,
        inputs: &[StoredSafeInput],
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current = query_current_safe_block(&tx)?;
        assert!(
            safe_block >= current,
            "safe block regressed: current={current}, next={safe_block}"
        );
        assert!(
            safe_block > current || inputs.is_empty(),
            "safe block must advance when appending new safe inputs"
        );

        let next_index = query_latest_safe_input_index_exclusive(&tx)?;
        insert_safe_inputs_batch(&tx, next_index, inputs)?;

        let changed = tx.execute(
            "UPDATE l1_safe_head SET block_number = ?1, synced_at_ms = ?2 WHERE singleton_id = 0",
            params![u64_to_i64(safe_block), now_unix_ms()],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }

        tx.commit()?;
        Ok(())
    }

    /// Wall-clock timestamp (Unix ms) of the last successful L1 sync. Returns 0
    /// if no sync has occurred. Read by the recovery wall-clock danger estimate.
    pub fn last_l1_sync_ms(&self) -> Result<u64> {
        let value: i64 = self.conn.query_row(
            "SELECT synced_at_ms FROM l1_safe_head WHERE singleton_id = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(i64_to_u64(value))
    }

    /// Read cached L1 bootstrap data (input_box_address, genesis_block, chain_id).
    /// Returns `None` on first startup.
    pub fn load_l1_bootstrap_cache(&self) -> Result<Option<(Address, u64, u64)>> {
        let row: Option<(Vec<u8>, i64, i64)> = self
            .conn
            .query_row(
                "SELECT input_box_address, genesis_block, chain_id \
                 FROM l1_bootstrap_cache WHERE singleton_id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(row.map(|(addr_bytes, genesis, chain_id)| {
            let addr = Address::from_slice(&addr_bytes);
            (addr, i64_to_u64(genesis), i64_to_u64(chain_id))
        }))
    }

    /// Cache L1 bootstrap data so future startups can boot without L1.
    pub fn save_l1_bootstrap_cache(
        &mut self,
        input_box_address: Address,
        genesis_block: u64,
        chain_id: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO l1_bootstrap_cache \
             (singleton_id, input_box_address, genesis_block, chain_id) \
             VALUES (0, ?1, ?2, ?3)",
            params![
                input_box_address.as_slice(),
                u64_to_i64(genesis_block),
                u64_to_i64(chain_id),
            ],
        )?;
        Ok(())
    }
}

fn insert_safe_inputs_batch(
    tx: &Transaction<'_>,
    start_index: u64,
    inputs: &[StoredSafeInput],
) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO safe_inputs (safe_input_index, sender, payload, block_number) \
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (offset, input) in inputs.iter().enumerate() {
        stmt.execute(params![
            u64_to_i64(start_index.saturating_add(offset as u64)),
            input.sender.as_slice(),
            input.payload.as_slice(),
            u64_to_i64(input.block_number),
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput, test_helpers::temp_db};
    use alloy_primitives::Address;

    #[test]
    fn safe_input_api_uses_half_open_intervals() {
        let db = temp_db("safe-input-api");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        assert_eq!(storage.safe_input_end_exclusive().expect("safe head"), 0);
        let mut out = Vec::new();
        storage
            .fill_safe_inputs(SafeInputRange::new(0, 0), &mut out)
            .expect("query empty interval");
        assert!(out.is_empty());

        let inserted = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xa0],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xb1],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, inserted.as_slice())
            .expect("insert safe directs");

        assert_eq!(storage.safe_input_end_exclusive().expect("safe head"), 2);

        storage
            .fill_safe_inputs(SafeInputRange::new(0, 2), &mut out)
            .expect("query full interval");
        assert_eq!(out, inserted);

        storage
            .fill_safe_inputs(SafeInputRange::new(1, 1), &mut out)
            .expect("query empty half-open interval");
        assert!(out.is_empty());
    }

    #[test]
    fn ensure_minimum_safe_block_only_moves_forward() {
        let db = temp_db("ensure-min-safe-block");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        storage
            .ensure_minimum_safe_block(7)
            .expect("advance bootstrap safe head");
        assert_eq!(storage.current_safe_block().expect("read advanced"), 7);

        storage
            .ensure_minimum_safe_block(3)
            .expect("do not regress bootstrap safe head");
        assert_eq!(storage.current_safe_block().expect("read unchanged"), 7);
    }

    #[test]
    fn ensure_minimum_safe_block_does_not_record_l1_sync() {
        let db = temp_db("ensure-min-safe-block-no-sync");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        storage
            .ensure_minimum_safe_block(7)
            .expect("advance bootstrap safe head");
        assert_eq!(
            storage.last_l1_sync_ms().expect("read sync timestamp"),
            0,
            "bootstrap safe-head initialization must not count as a real L1 sync"
        );

        storage.touch_l1_sync().expect("record real L1 sync");
        let recorded_sync = storage.last_l1_sync_ms().expect("read sync timestamp");
        assert!(
            recorded_sync > 0,
            "touch_l1_sync should record wall-clock time"
        );

        storage
            .ensure_minimum_safe_block(9)
            .expect("advance bootstrap safe head again");
        assert_eq!(
            storage.last_l1_sync_ms().expect("read sync timestamp"),
            recorded_sync,
            "bootstrap safe-head updates must preserve the last real L1 sync timestamp"
        );
    }
}
