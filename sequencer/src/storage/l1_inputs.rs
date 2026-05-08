// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Input reader writer: ingests L1 InputBox events into `safe_inputs`,
//! advances `l1_safe_head`, and maintains the L1 bootstrap cache.
//!
//! Also exposes the read-side queries the input reader and other callers need
//! (current safe block, safe-input bounds, last safe-progress timestamp).

use alloy_primitives::Address;
use rusqlite::{OptionalExtension, Result, Transaction, params};

use super::Storage;
use super::StoredSafeInput;
use super::convert::{i64_to_u64, now_unix_ms, u64_to_i64};
use super::queries::{
    current_safe_block, current_safe_block_timestamp, last_safe_progress_ms,
    query_latest_safe_input_index_exclusive,
};
use super::safe_accepted_batches::populate_safe_accepted_batches;
use sequencer_core::protocol::ProtocolConfig;

impl Storage {
    /// `MAX(safe_input_index) + 1` (or 0 if empty). The exclusive bound on the
    /// `safe_inputs` table — the next index a fresh row would receive.
    pub fn safe_input_end_exclusive(&mut self) -> Result<u64> {
        query_latest_safe_input_index_exclusive(&self.conn)
    }

    pub fn current_safe_block(&mut self) -> Result<Option<u64>> {
        current_safe_block(&self.conn)
    }

    pub fn current_safe_block_timestamp(&mut self) -> Result<Option<u64>> {
        current_safe_block_timestamp(&self.conn)
    }

    /// Atomically: insert `inputs` (assigned contiguous indexes starting from
    /// the current MAX+1), advance `l1_safe_head.block_number` to `safe_block`,
    /// stamp `synced_at_ms` as the wall-clock time when the safe frontier
    /// advanced, and update `safe_accepted_batches` via `protocol` so the
    /// scheduler-accepted frontier view stays consistent with the safe head.
    ///
    /// The materialized `safe_accepted_batches` view is an invariant of this
    /// operation: after a successful `append_safe_inputs`, every safe input up
    /// to `safe_block` has been evaluated against the scheduler's acceptance
    /// rules and recorded in `safe_accepted_batches`. Readers (submitter,
    /// recovery, danger checks) never need to populate separately.
    ///
    /// Asserts `safe_block` is monotonic and that it strictly advances when
    /// `inputs` is non-empty.
    pub fn append_safe_inputs(
        &mut self,
        safe_block: u64,
        inputs: &[StoredSafeInput],
        protocol: &ProtocolConfig,
    ) -> Result<()> {
        self.append_safe_inputs_with_timestamp(
            safe_block,
            i64_to_u64(now_unix_ms()) / 1000,
            inputs,
            protocol,
        )
    }

    /// Same as [`Storage::append_safe_inputs`], but records the L1 timestamp
    /// of `safe_block`. Production input-reader code should use this path;
    /// the shorter helper exists for tests that only need a fresh synthetic
    /// safe head.
    pub fn append_safe_inputs_with_timestamp(
        &mut self,
        safe_block: u64,
        safe_block_timestamp: u64,
        inputs: &[StoredSafeInput],
        protocol: &ProtocolConfig,
    ) -> Result<()> {
        self.write(|tx| {
            if let Some(current) = current_safe_block(tx)? {
                assert!(
                    safe_block >= current,
                    "safe block regressed: current={current}, next={safe_block}"
                );
                assert!(
                    safe_block > current || inputs.is_empty(),
                    "safe block must advance when appending new safe inputs"
                );
            }

            let next_index = query_latest_safe_input_index_exclusive(tx)?;
            insert_safe_inputs_batch(tx, next_index, inputs)?;

            let changed = tx.execute(
                "INSERT INTO l1_safe_head \
                    (singleton_id, block_number, block_timestamp, synced_at_ms) \
                 VALUES (0, ?1, ?2, ?3) \
                 ON CONFLICT(singleton_id) DO UPDATE SET \
                    block_number = excluded.block_number, \
                    block_timestamp = excluded.block_timestamp, \
                    synced_at_ms = excluded.synced_at_ms",
                params![
                    u64_to_i64(safe_block),
                    u64_to_i64(safe_block_timestamp),
                    now_unix_ms()
                ],
            )?;
            if changed != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed));
            }

            populate_safe_accepted_batches(tx, protocol)
        })
    }

    /// Wall-clock timestamp (Unix ms) of the last observed safe-head advance,
    /// or `None` if no real safe-head observation has occurred yet.
    pub fn last_safe_progress_ms(&self) -> Result<Option<u64>> {
        last_safe_progress_ms(&self.conn)
    }

    /// Read cached L1 bootstrap data (input_box_address, genesis_block, chain_id).
    /// Returns `None` on first startup.
    pub fn l1_bootstrap_cache(&self) -> Result<Option<(Address, u64, u64)>> {
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
    use crate::storage::{
        SafeInputRange, Storage, StoredSafeInput,
        test_helpers::{default_protocol_config, temp_db},
    };
    use alloy_primitives::Address;

    #[test]
    fn safe_input_api_uses_half_open_intervals() {
        let db = temp_db("safe-input-api");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_config();

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
            .append_safe_inputs(10, inserted.as_slice(), &protocol)
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
    fn new_db_has_no_observed_safe_head() {
        let db = temp_db("new-db-no-safe-head");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        assert_eq!(
            storage.current_safe_block().expect("read safe block"),
            None,
            "fresh storage should not pretend to have observed L1"
        );
        assert_eq!(
            storage
                .current_safe_block_timestamp()
                .expect("read block timestamp"),
            None,
            "fresh storage should not have a safe block timestamp"
        );
        assert_eq!(
            storage
                .last_safe_progress_ms()
                .expect("read sync timestamp"),
            None,
            "fresh storage should not have a safe-progress timestamp"
        );
    }

    #[test]
    fn append_safe_inputs_creates_and_advances_safe_head() {
        let db = temp_db("append-safe-inputs-creates-safe-head");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_config();

        storage
            .append_safe_inputs_with_timestamp(7, 1234, &[], &protocol)
            .expect("record first real safe-head observation");
        assert_eq!(
            storage.current_safe_block().expect("read safe block"),
            Some(7),
            "append should create the safe-head row"
        );
        let recorded_sync = storage
            .last_safe_progress_ms()
            .expect("read sync timestamp")
            .expect("first observation should record wall-clock time");
        assert_eq!(
            storage
                .current_safe_block_timestamp()
                .expect("read block timestamp"),
            Some(1234),
            "first observation should record the L1 safe block timestamp"
        );

        storage
            .append_safe_inputs_with_timestamp(9, 5678, &[], &protocol)
            .expect("advance safe head");
        assert_eq!(
            storage.current_safe_block().expect("read safe block"),
            Some(9),
            "append should advance the safe-head row"
        );
        assert_eq!(
            storage
                .current_safe_block_timestamp()
                .expect("read advanced block timestamp"),
            Some(5678),
            "append should record the observed L1 block timestamp"
        );
        assert!(
            storage
                .last_safe_progress_ms()
                .expect("read sync timestamp")
                .expect("advanced observation should record wall-clock time")
                >= recorded_sync,
            "safe-progress timestamp should stay monotonic across appends"
        );
    }
}
