// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Input reader writer: ingests L1 InputBox events into `safe_inputs`,
//! advances `l1_safe_head`, and pins the deployment identity.
//!
//! Also exposes the read-side queries the input reader and other callers need
//! (current safe block, safe-input bounds, last safe-progress timestamp).

use alloy_primitives::Address;
#[cfg(test)]
use alloy_primitives::B256;
use rusqlite::{OptionalExtension, Result, Transaction, params};

use super::Storage;
use super::convert::{
    external_u64_to_i64, i64_to_u64, now_unix_ms, saturating_query_bound, u64_to_i64,
};
use super::queries::{
    current_safe_block, current_safe_block_timestamp, last_safe_progress_ms,
    query_latest_safe_input_index_exclusive,
};
use super::safe_accepted_batches::populate_safe_accepted_batches;
use super::{
    DeploymentIdentity, FeeOracleIdentity, FrontierMode, IngestedSafeInput, StoredSafeInput,
};
use sequencer_core::protocol::ProtocolTiming;

type FeeOracleIdentitySql = (
    &'static str,
    Option<i64>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<i64>,
);

/// Test-fixture conversion: a provenance-free [`StoredSafeInput`] becomes a
/// row with explicit zero provenance. Production writes go through
/// [`Storage::append_ingested_safe_inputs_with_timestamp`] with real
/// provenance only — one honest row model, no trait shim (H6; the Track 6
/// inventory recorded the old `SafeInputRecord` shim as churn-avoidance).
#[cfg(test)]
fn synthetic_row(input: &StoredSafeInput) -> IngestedSafeInput {
    IngestedSafeInput {
        sender: input.sender,
        payload: input.payload.clone(),
        block_number: input.block_number,
        block_timestamp: 0,
        transaction_hash: B256::ZERO,
    }
}

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

    /// First batch-submitter `safe_inputs` row strictly past `after_block`, by
    /// ascending `safe_input_index` — returns `(safe_input_index, block_number)`
    /// or `None`. Read-only; `setup`'s detection gate uses it
    /// to find any previous-instance batch past the checkpoint block.
    ///
    /// Queries the reader-synced `safe_inputs` table rather than issuing its own
    /// `get_logs`, so it inherits the reader's F5 completeness guarantees (a
    /// per-app index contiguity check plus a `getNumberOfInputs` count witness):
    /// the reader refuses to persist an incomplete `get_logs` response, so the
    /// synced table is complete through the safe head. Do **not** replace this
    /// with a fresh log scan, which would bypass that protection. Detection runs
    /// after `setup`'s initial sync has populated `safe_inputs` up to the safe
    /// head;
    /// because step 1 has already confirmed the wallet nonce is settled
    /// (nothing of ours sits unsafe), querying the synced safe inputs is
    /// equivalent to scanning `(after_block, safe]`.
    pub fn first_batch_submitter_input_after_block(
        &mut self,
        batch_submitter: Address,
        after_block: u64,
    ) -> Result<Option<(u64, u64)>> {
        self.conn
            .query_row(
                "SELECT safe_input_index, block_number FROM safe_inputs \
                 WHERE sender = ?1 AND block_number > ?2 \
                 ORDER BY safe_input_index ASC LIMIT 1",
                params![
                    batch_submitter.as_slice(),
                    saturating_query_bound(after_block)
                ],
                |row| {
                    let index: i64 = row.get(0)?;
                    let block: i64 = row.get(1)?;
                    Ok((i64_to_u64(index), i64_to_u64(block)))
                },
            )
            .optional()
    }

    /// All `safe_inputs` rows whose `block_number` is in `(after_block,
    /// through_block]`, ordered by `safe_input_index` ascending — i.e. L1
    /// inclusion order. The recovery fold sources
    /// its seeds from `(A, B]` and its replay stream from `(B, C]` through this:
    /// half-open on the lower bound (directs at block `B` belong to the fridge,
    /// batches at `B` are already in `S` — open-question 1 boundary convention).
    ///
    /// Returns both senders and directs; the caller classifies (a batch iff
    /// `sender == batch_submitter`) and drops batches from the `(A, B]` seed set.
    /// Read-only; queries the reader-synced table rather than a fresh log scan,
    /// inheriting the reader's F5 completeness guarantees (see
    /// [`Storage::first_batch_submitter_input_after_block`]).
    pub fn safe_inputs_in_block_range(
        &mut self,
        after_block: u64,
        through_block: u64,
    ) -> Result<Vec<StoredSafeInput>> {
        const SQL: &str = "SELECT sender, payload, block_number FROM safe_inputs \
                           WHERE block_number > ?1 AND block_number <= ?2 \
                           ORDER BY safe_input_index ASC";
        let mut stmt = self.conn.prepare_cached(SQL)?;
        let rows = stmt.query_map(
            params![
                saturating_query_bound(after_block),
                saturating_query_bound(through_block)
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (sender, payload, block_number) = row?;
            out.push(StoredSafeInput {
                sender: Address::from_slice(sender.as_slice()),
                payload,
                block_number: i64_to_u64(block_number),
            });
        }
        Ok(out)
    }

    /// Test seed: [`Storage::append_safe_inputs_with_timestamp`] at the
    /// current wall clock. See
    /// [`Storage::append_ingested_safe_inputs_with_timestamp`] for the
    /// operation's contract.
    #[cfg(test)]
    pub(crate) fn append_safe_inputs(
        &mut self,
        safe_block: u64,
        inputs: &[StoredSafeInput],
        batch_submitter: Address,
        timing: &ProtocolTiming,
    ) -> Result<()> {
        self.append_safe_inputs_with_timestamp(
            safe_block,
            i64_to_u64(now_unix_ms()) / 1000,
            inputs,
            batch_submitter,
            timing,
            FrontierMode::Populate,
        )
    }

    /// Same as [`Storage::append_safe_inputs`], but records the L1 timestamp
    /// of `safe_block`. Test seed only: synthetic inputs receive explicit zero
    /// provenance via [`synthetic_row`]; the production write path is
    /// [`Storage::append_ingested_safe_inputs_with_timestamp`].
    ///
    /// `frontier` gates the `safe_accepted_batches` update — see
    /// [`FrontierMode`]. Everything except `setup --recovery`'s interim syncs
    /// uses [`FrontierMode::Populate`].
    #[cfg(test)]
    pub(crate) fn append_safe_inputs_with_timestamp(
        &mut self,
        safe_block: u64,
        safe_block_timestamp: u64,
        inputs: &[StoredSafeInput],
        batch_submitter: Address,
        timing: &ProtocolTiming,
        frontier: FrontierMode,
    ) -> Result<()> {
        let rows: Vec<IngestedSafeInput> = inputs.iter().map(synthetic_row).collect();
        self.append_ingested_safe_inputs_with_timestamp(
            safe_block,
            safe_block_timestamp,
            rows.as_slice(),
            batch_submitter,
            timing,
            frontier,
        )
    }

    /// Production input-reader path — the one write of the safe-input stream.
    ///
    /// Atomically: insert `inputs` (assigned contiguous indexes starting from
    /// the current MAX+1) with their per-input L1 provenance, advance
    /// `l1_safe_head.block_number` to `safe_block`, stamp `synced_at_ms` as
    /// the wall-clock time when the safe frontier advanced, and update
    /// `safe_accepted_batches` via `timing` so the scheduler-accepted
    /// frontier view stays consistent with the safe head.
    ///
    /// The materialized `safe_accepted_batches` view is an invariant of this
    /// operation while no divergence exists: every safe input up to
    /// `safe_block` has been evaluated against the scheduler's acceptance
    /// rules. A foreign/mismatched accepted landing instead commits the
    /// `canonical_divergence` fact with this head and freezes the projection;
    /// later head advances remain paired with that terminal fact. Readers
    /// (submitter, recovery, danger checks) never populate separately.
    ///
    /// Asserts `safe_block` is monotonic and that it strictly advances when
    /// `inputs` is non-empty.
    pub(crate) fn append_ingested_safe_inputs_with_timestamp(
        &mut self,
        safe_block: u64,
        safe_block_timestamp: u64,
        inputs: &[IngestedSafeInput],
        batch_submitter: Address,
        timing: &ProtocolTiming,
        frontier: FrontierMode,
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
                    external_u64_to_i64(safe_block, "L1 safe block")?,
                    external_u64_to_i64(safe_block_timestamp, "L1 safe block timestamp")?,
                    now_unix_ms()
                ],
            )?;
            if changed != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed));
            }

            if matches!(frontier, FrontierMode::Populate) {
                populate_safe_accepted_batches(tx, batch_submitter, timing)?;
            }
            Ok(())
        })
    }

    /// Wall-clock timestamp (Unix ms) of the last observed safe-head advance,
    /// or `None` if no real safe-head observation has occurred yet.
    pub fn last_safe_progress_ms(&self) -> Result<Option<u64>> {
        last_safe_progress_ms(&self.conn)
    }

    /// Read the deployment identity this DB is pinned to. Returns `None` on
    /// first startup, before L1 bootstrap has discovered the InputBox stream.
    pub fn deployment_identity(&self) -> Result<Option<DeploymentIdentity>> {
        query_deployment_identity(&self.conn)
    }

    /// Whether this DB already contains deployment-bound state. Used to avoid
    /// silently pinning an old, non-empty DB that predates `deployment_identity`
    /// to whatever config happens to start it next.
    pub fn has_persisted_deployment_state(&self) -> Result<bool> {
        let present: i64 = self.conn.query_row(
            "SELECT \
                EXISTS(SELECT 1 FROM batches) OR \
                EXISTS(SELECT 1 FROM safe_inputs) OR \
                EXISTS(SELECT 1 FROM l1_safe_head)",
            [],
            |row| row.get(0),
        )?;
        Ok(present != 0)
    }

    /// Insert `identity` on first startup, or return the already-persisted
    /// identity on later startups. The caller compares the returned value with
    /// its configured/discovered identity and refuses on mismatch.
    pub fn load_or_insert_deployment_identity(
        &mut self,
        identity: DeploymentIdentity,
    ) -> Result<DeploymentIdentity> {
        self.write(|tx| {
            if let Some(existing) = query_deployment_identity(tx)? {
                return Ok(existing);
            }

            let (mode, fixed, weth, fee_token, pool, window): FeeOracleIdentitySql =
                match identity.fee_oracle {
                    FeeOracleIdentity::Fixed { log_gas_price } => (
                        "fixed",
                        Some(i64::from(log_gas_price)),
                        None,
                        None,
                        None,
                        None,
                    ),
                    FeeOracleIdentity::Uniswap {
                        weth,
                        fee_token,
                        pool,
                        twap_window_secs,
                    } => (
                        "uniswap",
                        None,
                        Some(weth.as_slice().to_vec()),
                        Some(fee_token.as_slice().to_vec()),
                        Some(pool.as_slice().to_vec()),
                        Some(i64::from(twap_window_secs)),
                    ),
                };
            let changed = tx.execute(
                "INSERT INTO deployment_identity \
                    (singleton_id, chain_id, app_address, input_box_address, \
                     app_deployment_block, batch_submitter_address, fee_oracle_mode, \
                     fixed_log_gas_price, fee_oracle_weth, fee_oracle_fee_token, \
                     fee_oracle_pool, fee_oracle_twap_window_secs) \
                 VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    external_u64_to_i64(identity.chain_id, "deployment chain ID")?,
                    identity.app_address.as_slice(),
                    identity.input_box_address.as_slice(),
                    external_u64_to_i64(
                        identity.app_deployment_block,
                        "application deployment block"
                    )?,
                    identity.batch_submitter_address.as_slice(),
                    mode,
                    fixed,
                    weth,
                    fee_token,
                    pool,
                    window,
                ],
            )?;
            if changed != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed));
            }
            Ok(identity)
        })
    }

    /// Whether `setup` has completed on this DB. `run` refuses to boot when
    /// this is `false`: setup either never ran or crashed midway, both of
    /// which require `setup` (re-)run, not `run`.
    pub fn is_setup_complete(&self) -> Result<bool> {
        let present: i64 = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM setup_complete WHERE singleton_id = 0)",
            [],
            |row| row.get(0),
        )?;
        Ok(present != 0)
    }
}

pub(super) fn query_deployment_identity(
    conn: &rusqlite::Connection,
) -> Result<Option<DeploymentIdentity>> {
    conn.query_row(
        "SELECT chain_id, app_address, input_box_address, \
                app_deployment_block, batch_submitter_address, fee_oracle_mode, \
                fixed_log_gas_price, fee_oracle_weth, fee_oracle_fee_token, \
                fee_oracle_pool, fee_oracle_twap_window_secs \
         FROM deployment_identity WHERE singleton_id = 0",
        [],
        |row| {
            let mode: String = row.get(5)?;
            let fee_oracle = match mode.as_str() {
                "fixed" => FeeOracleIdentity::Fixed {
                    log_gas_price: row.get(6)?,
                },
                "uniswap" => FeeOracleIdentity::Uniswap {
                    weth: Address::from_slice(&row.get::<_, Vec<u8>>(7)?),
                    fee_token: Address::from_slice(&row.get::<_, Vec<u8>>(8)?),
                    pool: Address::from_slice(&row.get::<_, Vec<u8>>(9)?),
                    twap_window_secs: row.get(10)?,
                },
                _ => panic!("schema CHECK prevents unknown fee oracle mode"),
            };
            Ok(DeploymentIdentity {
                chain_id: i64_to_u64(row.get::<_, i64>(0)?),
                app_address: Address::from_slice(&row.get::<_, Vec<u8>>(1)?),
                input_box_address: Address::from_slice(&row.get::<_, Vec<u8>>(2)?),
                app_deployment_block: i64_to_u64(row.get::<_, i64>(3)?),
                batch_submitter_address: Address::from_slice(&row.get::<_, Vec<u8>>(4)?),
                fee_oracle,
            })
        },
    )
    .optional()
}

fn insert_safe_inputs_batch(
    tx: &Transaction<'_>,
    start_index: u64,
    inputs: &[IngestedSafeInput],
) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO safe_inputs \
            (safe_input_index, sender, payload, block_number, block_timestamp, transaction_hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (offset, input) in inputs.iter().enumerate() {
        let offset = u64::try_from(offset)
            .expect("safe-input batch offset exceeds u64: contract-impossible");
        stmt.execute(params![
            u64_to_i64(
                start_index
                    .checked_add(offset)
                    .expect("safe-input index overflow: contract-impossible"),
            ),
            input.sender.as_slice(),
            input.payload.as_slice(),
            external_u64_to_i64(input.block_number, "safe-input L1 block")?,
            external_u64_to_i64(input.block_timestamp, "safe-input L1 timestamp")?,
            input.transaction_hash.as_slice(),
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::storage::{
        DeploymentIdentity, FeeOracleIdentity, FrontierMode, IngestedSafeInput, SafeInputRange,
        Storage, StoredSafeInput,
        test_helpers::{SENDER_A, SENDER_B, default_protocol_timing, temp_db},
    };
    use alloy_primitives::{Address, B256};

    fn identity() -> DeploymentIdentity {
        DeploymentIdentity {
            chain_id: 31337,
            app_address: Address::repeat_byte(0x11),
            input_box_address: Address::repeat_byte(0x22),
            app_deployment_block: 42,
            batch_submitter_address: SENDER_A,
            fee_oracle: FeeOracleIdentity::Fixed { log_gas_price: 0 },
        }
    }

    #[test]
    fn safe_input_api_uses_half_open_intervals() {
        let db = temp_db("safe-input-api");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_timing();

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
            .append_safe_inputs(10, inserted.as_slice(), SENDER_A, &protocol)
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
    fn first_batch_submitter_input_after_block_scans_strictly_past() {
        let db = temp_db("first-submitter-input-after");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_timing();

        // index 0: submitter @5; index 1: non-submitter @12; index 2: submitter @18.
        // Payloads are junk (scheduler no-ops on decode) — detection scans the
        // raw safe_inputs rows, not the accepted frontier, so acceptance is
        // irrelevant: ANY submitter activity past the checkpoint matters.
        let inputs = vec![
            StoredSafeInput {
                sender: SENDER_A,
                payload: vec![0x01],
                block_number: 5,
            },
            StoredSafeInput {
                sender: SENDER_B,
                payload: vec![0x02],
                block_number: 12,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: vec![0x03],
                block_number: 18,
            },
        ];
        storage
            .append_safe_inputs(20, inputs.as_slice(), SENDER_A, &protocol)
            .expect("seed safe inputs");

        // From genesis (0): the earliest submitter input is index 0 @5.
        assert_eq!(
            storage
                .first_batch_submitter_input_after_block(SENDER_A, 0)
                .expect("scan"),
            Some((0, 5))
        );
        // Strictly past: block 5 is excluded; the non-submitter @12 is skipped;
        // next submitter is index 2 @18.
        assert_eq!(
            storage
                .first_batch_submitter_input_after_block(SENDER_A, 5)
                .expect("scan"),
            Some((2, 18))
        );
        // Past the last submitter input: nothing.
        assert_eq!(
            storage
                .first_batch_submitter_input_after_block(SENDER_A, 18)
                .expect("scan"),
            None
        );
        // A different submitter address never matches SENDER_A's rows.
        assert_eq!(
            storage
                .first_batch_submitter_input_after_block(Address::repeat_byte(0xCC), 0)
                .expect("scan"),
            None
        );
        // Config accepts the full u64 range. Bounds above SQLite's INTEGER
        // range are past every representable block and must not panic.
        assert_eq!(
            storage
                .first_batch_submitter_input_after_block(SENDER_A, u64::MAX)
                .expect("scan past SQLite range"),
            None
        );
    }

    #[test]
    fn safe_inputs_in_block_range_is_half_open_lower_and_ordered() {
        let db = temp_db("safe-inputs-block-range");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_timing();

        // Blocks 5 (direct), 10 (batch from SENDER_A + direct from SENDER_B),
        // 15 (direct). SENDER_A is the batch submitter (identity above).
        let inputs = vec![
            StoredSafeInput {
                sender: SENDER_B,
                payload: vec![0x05],
                block_number: 5,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: vec![0x10],
                block_number: 10,
            },
            StoredSafeInput {
                sender: SENDER_B,
                payload: vec![0x11],
                block_number: 10,
            },
            StoredSafeInput {
                sender: SENDER_B,
                payload: vec![0x15],
                block_number: 15,
            },
        ];
        storage
            .append_safe_inputs(15, inputs.as_slice(), SENDER_A, &protocol)
            .expect("seed safe inputs");

        // (5, 15] excludes block 5, includes block 15 — ordered by index.
        let mid = storage
            .safe_inputs_in_block_range(5, 15)
            .expect("range (5,15]");
        let blocks: Vec<u64> = mid.iter().map(|i| i.block_number).collect();
        assert_eq!(blocks, vec![10, 10, 15], "half-open lower bound; ascending");

        // Caller classifies: drop sender == batch_submitter to get the directs
        // of the seed range (the (A,B] fridge reconstruction).
        let directs: Vec<&[u8]> = mid
            .iter()
            .filter(|i| i.sender != SENDER_A)
            .map(|i| i.payload.as_slice())
            .collect();
        assert_eq!(
            directs,
            vec![&[0x11u8][..], &[0x15u8][..]],
            "batch at block 10 dropped"
        );

        // Empty when the lower bound covers everything.
        assert!(
            storage
                .safe_inputs_in_block_range(15, 15)
                .expect("empty")
                .is_empty()
        );
        // Full span from genesis.
        assert_eq!(
            storage
                .safe_inputs_in_block_range(0, 100)
                .expect("all")
                .len(),
            4
        );
        assert_eq!(
            storage
                .safe_inputs_in_block_range(0, u64::MAX)
                .expect("all through saturated upper bound")
                .len(),
            4,
            "an upper bound above SQLite's range includes every stored row"
        );
        assert!(
            storage
                .safe_inputs_in_block_range(u64::MAX, u64::MAX)
                .expect("empty past SQLite range")
                .is_empty(),
            "a lower bound above SQLite's range excludes every stored row"
        );
    }

    #[test]
    fn batch_tree_anchor_roundtrips_and_freezes_after_setup() {
        let db = temp_db("anchor-roundtrip");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        assert_eq!(storage.batch_tree_anchor().expect("default"), 0);
        storage.set_batch_tree_anchor(1200).expect("set anchor");
        assert_eq!(storage.batch_tree_anchor().expect("read back"), 1200);
        storage
            .conn
            .execute(
                "INSERT INTO setup_complete (singleton_id, completed_at_ms) VALUES (0, 1)",
                [],
            )
            .expect("seed setup completion fact");
        assert!(
            storage.set_batch_tree_anchor(1300).is_err(),
            "anchor must be frozen after setup_complete"
        );
        assert_eq!(storage.batch_tree_anchor().expect("unchanged"), 1200);
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
    fn external_values_outside_sqlite_integer_range_are_typed_refusals() {
        let db = temp_db("external-integer-range");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_timing();

        let safe_head_err = storage
            .append_safe_inputs_with_timestamp(
                u64::MAX,
                1,
                &[],
                SENDER_A,
                &protocol,
                FrontierMode::Populate,
            )
            .expect_err("unrepresentable provider safe block must be refused");
        assert!(matches!(
            safe_head_err,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));
        let safe_timestamp_err = storage
            .append_safe_inputs_with_timestamp(
                10,
                u64::MAX,
                &[],
                SENDER_A,
                &protocol,
                FrontierMode::Populate,
            )
            .expect_err("unrepresentable provider safe timestamp must be refused");
        assert!(matches!(
            safe_timestamp_err,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));

        let input_err = storage
            .append_safe_inputs(
                10,
                &[StoredSafeInput {
                    sender: SENDER_B,
                    payload: vec![],
                    block_number: u64::MAX,
                }],
                SENDER_A,
                &protocol,
            )
            .expect_err("unrepresentable provider input block must be refused");
        assert!(matches!(
            input_err,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));
        let input_timestamp_err = storage
            .append_ingested_safe_inputs_with_timestamp(
                10,
                1,
                &[IngestedSafeInput {
                    sender: SENDER_B,
                    payload: vec![],
                    block_number: 10,
                    block_timestamp: u64::MAX,
                    transaction_hash: B256::ZERO,
                }],
                SENDER_A,
                &protocol,
                FrontierMode::Populate,
            )
            .expect_err("unrepresentable provider input timestamp must be refused");
        assert!(matches!(
            input_timestamp_err,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));
        assert_eq!(
            storage.current_safe_block().expect("read safe head"),
            None,
            "failed external writes must roll back atomically"
        );
        assert_eq!(
            storage.safe_input_end_exclusive().expect("read input head"),
            0
        );

        let identity_err = storage
            .load_or_insert_deployment_identity(DeploymentIdentity {
                chain_id: u64::MAX,
                ..identity()
            })
            .expect_err("unrepresentable configured chain ID must be refused");
        assert!(matches!(
            identity_err,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));
        let deployment_block_err = storage
            .load_or_insert_deployment_identity(DeploymentIdentity {
                app_deployment_block: u64::MAX,
                ..identity()
            })
            .expect_err("unrepresentable configured application deployment block must be refused");
        assert!(matches!(
            deployment_block_err,
            rusqlite::Error::ToSqlConversionFailure(_)
        ));
        assert_eq!(storage.deployment_identity().expect("read identity"), None);
    }

    #[test]
    fn deployment_identity_is_inserted_once() {
        let db = temp_db("deployment-identity-insert-once");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let first = identity();

        assert_eq!(
            storage.deployment_identity().expect("read empty identity"),
            None
        );
        assert_eq!(
            storage
                .load_or_insert_deployment_identity(first)
                .expect("insert identity"),
            first
        );
        assert_eq!(
            storage
                .deployment_identity()
                .expect("read persisted identity"),
            Some(first)
        );

        let changed = DeploymentIdentity {
            batch_submitter_address: SENDER_B,
            ..first
        };
        assert_eq!(
            storage
                .load_or_insert_deployment_identity(changed)
                .expect("load existing identity"),
            first,
            "identity must be pinned after the first insert"
        );
        assert_eq!(
            storage
                .deployment_identity()
                .expect("read persisted identity"),
            Some(first)
        );
    }

    #[test]
    fn append_safe_inputs_creates_and_advances_safe_head() {
        let db = temp_db("append-safe-inputs-creates-safe-head");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_timing();

        storage
            .append_safe_inputs_with_timestamp(
                7,
                1234,
                &[],
                SENDER_A,
                &protocol,
                FrontierMode::Populate,
            )
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
            .append_safe_inputs_with_timestamp(
                9,
                5678,
                &[],
                SENDER_A,
                &protocol,
                FrontierMode::Populate,
            )
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
