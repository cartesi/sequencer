// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Steps 4 to 6 of setup, over an input table synced through the stop block:
//! check the base's export against L1, then rebuild the history with the
//! scheduler, executing each transaction as it is handed out. Inputs and rows
//! move in pages, so memory stays bounded however long the range is.

use alloy_primitives::Address;
use sequencer_core::application::{Application, Executed, execute};
use sequencer_core::history::L2TxCount;
use sequencer_core::l2_tx::L2Tx;
use sequencer_core::scheduler::{
    L1Input, Scheduled, Scheduler, SchedulerConfig, SchedulerExport, SchedulerState,
};

use crate::commands::error::{CommandError, SetupError};
use crate::storage::{SafeInputRange, Storage};

/// Inputs and rows per page. Tests use a tiny page, so their few rows
/// already cross page boundaries inside one input and inside the resume range.
#[cfg(not(test))]
const PAGE: usize = 1024;
#[cfg(test)]
const PAGE: usize = 2;

/// Step 4: among the inputs the base consumed, the admitted directs above its
/// clock must be exactly the ones its export queues, and the base must lie at
/// or below the stop block. `inputs_through_stop` is the L1 input count
/// through the stop block.
pub(crate) fn check_export(
    storage: &mut Storage,
    export: &SchedulerExport,
    inputs_through_stop: u64,
    stop_block: u64,
    max_direct_payload_bytes: u64,
) -> Result<(), CommandError> {
    if export.l1_input_count > inputs_through_stop {
        return Err(SetupError::BaseAheadOfStop {
            l1_input_count: export.l1_input_count,
            inputs_through_stop,
            stop_block,
        }
        .into());
    }
    let admitted = storage.admitted_directs_above_clock(
        export.l1_input_count,
        export.clock,
        max_direct_payload_bytes,
    )?;
    let starts_at_first_queued = admitted
        .first_index
        .is_none_or(|index| index == export.first_queued_input);
    if admitted.count != export.fridge_len || !starts_at_first_queued {
        return Err(SetupError::ExportDisagrees {
            l1_input_count: export.l1_input_count,
            clock: export.clock,
            fridge_len: export.fridge_len,
            first_queued_input: export.first_queued_input,
            admitted: admitted.count,
            first_admitted: admitted.first_index,
        }
        .into());
    }
    Ok(())
}

/// The rest of step 4: resume the scheduler from the export over the inputs
/// it still had queued. Its refusals are the export check's, so the dry run
/// runs this too. Reads only the queued range and writes nothing.
pub(crate) fn resume_from_export(
    storage: &mut Storage,
    config: SchedulerConfig,
    export: &SchedulerExport,
) -> Result<Scheduler, CommandError> {
    let state = export.scheduler_state();
    let mut queued = InputPages::new(storage, state.first_queued_input, state.l1_input_count);
    let resumed = Scheduler::resume(config, state, &mut queued);
    // A failed read ends the range early; report it, not the short range.
    if let Some(error) = queued.error {
        return Err(error.into());
    }
    Ok(resumed.map_err(SetupError::ResumeRefused)?)
}

/// The scheduler's clock is a safe block a frame named, so it never lies past
/// the block the scheduler has consumed through.
pub(crate) fn check_clock(state: &SchedulerState, stop_block: u64) -> Result<(), CommandError> {
    if state.clock > stop_block {
        return Err(SetupError::ClockPastStop {
            clock: state.clock,
            stop_block,
        }
        .into());
    }
    Ok(())
}

/// Steps 5 and 6: resume the scheduler from the export with the inputs it
/// still had queued, feed it every input it has not consumed through the stop
/// block, and execute each transaction it hands out through the shared executor.
/// An executed one is stored with its count; a skipped user op is not. An
/// application error stops setup at that transaction. Returns the
/// scheduler's state at the end of the stop block.
pub(crate) fn rebuild_history<A: Application>(
    storage: &mut Storage,
    app: &mut A,
    config: SchedulerConfig,
    export: &SchedulerExport,
    inputs_through_stop: u64,
    stop_block: u64,
) -> Result<SchedulerState, CommandError> {
    let mut scheduler = resume_from_export(storage, config, export)?;
    check_clock(&scheduler.state(), stop_block)?;

    let mut rows: Vec<(L2TxCount, Scheduled)> = Vec::with_capacity(PAGE);
    let mut next = export.l1_input_count;
    while next < inputs_through_stop {
        let page = storage.l1_inputs_page(next, inputs_through_stop, PAGE)?;
        assert!(
            !page.is_empty(),
            "the input table ends at {next}, below the {inputs_through_stop} inputs it counted \
             through the stop block"
        );
        for input in page {
            let l1_input_index = input.index;
            scheduler.feed(input);
            // Every transaction the input released, skipped ones included.
            let mut position: u64 = 0;
            while let Some(scheduled) = scheduler.next_tx() {
                let executed = execute(app, &scheduled.tx);
                match (executed, &scheduled.tx) {
                    (Ok(Executed::Done { index, .. }), _) => {
                        rows.push((index, scheduled));
                        if rows.len() == PAGE {
                            storage.append_rebuilt_history(&rows)?;
                            rows.clear();
                        }
                    }
                    (Ok(Executed::Skipped(_)), L2Tx::UserOp(_)) => {}
                    (Ok(Executed::Skipped(_)), L2Tx::Direct(_)) => {
                        unreachable!("a direct never skips")
                    }
                    (Err(source), tx) => {
                        return Err(SetupError::HistoryExecution {
                            l1_input_index,
                            position,
                            transaction: describe(tx),
                            source,
                        }
                        .into());
                    }
                }
                position = position
                    .checked_add(1)
                    .expect("an input's transaction count fits u64");
            }
            next = l1_input_index
                .checked_add(1)
                .expect("an input index has a successor");
        }
    }
    storage.append_rebuilt_history(&rows)?;

    let end = scheduler.state();
    check_clock(&end, stop_block)?;
    assert_drained_directs_stored(
        storage,
        export.first_queued_input,
        &end,
        config.max_direct_payload_bytes,
    )?;
    Ok(end)
}

/// The stored rows hold only what executed, so the directs are checked against
/// the input table. The queue is FIFO and holds every admitted direct from the
/// base's first queued input on, so the drained ones are exactly the admitted
/// inputs before the final first queued input, and a direct always executes:
/// the stored directs must be those inputs, each one and no other.
fn assert_drained_directs_stored(
    storage: &mut Storage,
    base_first_queued_input: u64,
    end: &SchedulerState,
    max_direct_payload_bytes: u64,
) -> Result<(), CommandError> {
    let drained = SafeInputRange::new(base_first_queued_input, end.first_queued_input);
    let admitted = storage.admitted_directs_in(drained, max_direct_payload_bytes)?;
    let rebuilt = storage.rebuilt_directs(drained, max_direct_payload_bytes)?;
    assert!(
        rebuilt.stored == admitted && rebuilt.admitted_in_range == admitted,
        "the rebuild stored {} directs, {} of them admitted inputs in [{base_first_queued_input}, {}), \
         where {admitted} admitted inputs lie",
        rebuilt.stored,
        rebuilt.admitted_in_range,
        end.first_queued_input
    );
    Ok(())
}

/// The failing transaction for an error: it has no row to point at.
fn describe(tx: &L2Tx) -> String {
    match tx {
        L2Tx::Direct(direct) => format!("direct input {}", direct.input_index),
        L2Tx::UserOp(op) => format!("user op of {} at nonce {}", op.sender, op.nonce),
    }
}

/// How many inputs in `[start, end)` came from `submitter`: the old
/// instance's activity past a base.
pub(crate) fn count_submitter_inputs(
    storage: &mut Storage,
    submitter: Address,
    start: u64,
    end: u64,
) -> Result<u64, CommandError> {
    let mut pages = InputPages::new(storage, start, end);
    let count = pages
        .by_ref()
        .filter(|input| input.sender == submitter)
        .count();
    if let Some(error) = pages.error {
        return Err(error.into());
    }
    Ok(u64::try_from(count).expect("a count of inputs fits u64"))
}

/// The inputs `[start, end)` from storage, a page at a time. A read failure
/// ends the iteration and is kept for the caller.
struct InputPages<'a> {
    storage: &'a mut Storage,
    next: u64,
    end: u64,
    page: std::vec::IntoIter<L1Input>,
    error: Option<rusqlite::Error>,
}

impl<'a> InputPages<'a> {
    fn new(storage: &'a mut Storage, start: u64, end: u64) -> Self {
        Self {
            storage,
            next: start,
            end,
            page: Vec::new().into_iter(),
            error: None,
        }
    }
}

impl Iterator for InputPages<'_> {
    type Item = L1Input;

    fn next(&mut self) -> Option<L1Input> {
        if let Some(input) = self.page.next() {
            return Some(input);
        }
        if self.error.is_some() || self.next >= self.end {
            return None;
        }
        match self.storage.l1_inputs_page(self.next, self.end, PAGE) {
            Ok(page) => {
                let last = page.last()?.index;
                self.next = last.checked_add(1).expect("an input index has a successor");
                self.page = page.into_iter();
                self.page.next()
            }
            Err(error) => {
                self.error = Some(error);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::default_protocol_timing;
    use crate::storage::{IngestedSafeInput, LifecycleCommand};
    use crate::test_support::{TestDb, pin_test_deployment_identity, temp_db};
    use alloy_primitives::B256;
    use app_core::application::{WalletApp, WalletConfig};
    use sequencer_core::application::{AppError, AppOutputs, CanonicalState};
    use sequencer_core::batch::{Batch, Frame, WireUserOp};
    use sequencer_core::history::L2TxCount;
    use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
    use sequencer_core::scheduler::{CanonicalRunner, TxFate};
    use ssz::Encode;

    const SUBMITTER: Address = Address::repeat_byte(0x99);
    const DEPOSITOR: Address = Address::repeat_byte(0x22);
    /// The chain and application `pin_test_deployment_identity` pins.
    const CHAIN_ID: u64 = 1;
    const APP: Address = Address::repeat_byte(0x11);

    fn wallet() -> WalletApp {
        WalletApp::new(WalletConfig {
            sequencer_address: SUBMITTER,
            ..WalletConfig::default()
        })
    }

    fn config() -> SchedulerConfig {
        SchedulerConfig::for_application::<WalletApp>(SUBMITTER)
    }

    fn cap() -> u64 {
        WalletApp::scheduler_limits().max_direct_payload_bytes
    }

    fn direct(block: u64, payload: Vec<u8>) -> IngestedSafeInput {
        IngestedSafeInput {
            sender: DEPOSITOR,
            payload,
            block_number: block,
            block_timestamp: 1_700_000_000 + block,
            transaction_hash: B256::repeat_byte(u8::try_from(block % 251).unwrap()),
        }
    }

    /// A batch from the submitter with one frame at `safe_block` and one
    /// signed op of `signer` at `op_nonce`.
    fn batch(nonce: u64, block: u64, safe_block: u64, op_nonce: u32) -> IngestedSafeInput {
        use alloy::signers::{SignerSync, local::PrivateKeySigner};
        use alloy_sol_types::SolStruct;
        let signer: PrivateKeySigner = format!("{:064x}", 7).parse().unwrap();
        let op = sequencer_core::user_op::UserOp {
            nonce: op_nonce,
            max_fee: 0,
            data: vec![0xde, 0xad].into(),
        };
        let domain = sequencer_core::build_input_domain(CHAIN_ID, APP);
        let signature = signer
            .sign_hash_sync(&op.eip712_signing_hash(&domain))
            .unwrap();
        IngestedSafeInput {
            sender: SUBMITTER,
            payload: Batch {
                nonce,
                frames: vec![Frame {
                    safe_block,
                    fee_price: 0,
                    user_ops: vec![WireUserOp {
                        nonce: op_nonce,
                        max_fee: 0,
                        data: op.data.to_vec(),
                        signature: signature.as_bytes().to_vec(),
                    }],
                }],
            }
            .as_ssz_bytes(),
            block_number: block,
            block_timestamp: 1_700_000_000 + block,
            transaction_hash: B256::repeat_byte(0xbb),
        }
    }

    fn l1_input(index: u64, input: &IngestedSafeInput) -> L1Input {
        L1Input {
            index,
            sender: input.sender,
            block: input.block_number,
            timestamp: input.block_timestamp,
            chain_id: CHAIN_ID,
            app_contract: APP,
            payload: input.payload.clone(),
        }
    }

    /// A setup database with `inputs` synced through `safe_block`.
    fn synced(name: &str, inputs: &[IngestedSafeInput], safe_block: u64) -> (TestDb, Storage) {
        let db = temp_db(name);
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
        pin_test_deployment_identity(&mut storage, SUBMITTER);
        storage
            .append_ingested_safe_inputs_with_timestamp(
                safe_block,
                1_700_000_000 + safe_block,
                inputs,
                &default_protocol_timing(),
            )
            .unwrap();
        (db, storage)
    }

    /// The chain the tests share: directs, an accepted batch that drains the
    /// first, an over-cap direct, a stale batch, a direct left queued, and a
    /// batch whose op is skipped for its nonce.
    fn chain() -> Vec<IngestedSafeInput> {
        vec![
            direct(10, vec![1]),
            direct(11, vec![2]),
            batch(0, 12, 10, 0),
            direct(13, vec![0; usize::try_from(cap()).unwrap() + 1]),
            direct(14, vec![3]),
            batch(0, 15, 13, 1),
            direct(16, vec![4]),
            batch(1, 17, 14, 5),
            direct(30, vec![5]),
        ]
    }

    /// Run the reference over `inputs[..count]`.
    fn reference(inputs: &[IngestedSafeInput], count: usize) -> CanonicalRunner<WalletApp> {
        let mut runner = CanonicalRunner::genesis(wallet(), config());
        for (index, input) in (0_u64..).zip(&inputs[..count]) {
            runner.process_input(l1_input(index, input)).unwrap();
        }
        runner
    }

    /// Every stored row with its count, in count order. A direct reads its
    /// fields back from the input table.
    fn stored(storage: &mut Storage) -> Vec<(L2TxCount, Scheduled)> {
        let to_u64 = |value: i64| u64::try_from(value).unwrap();
        storage
            .raw_read(|tx| {
                let mut statement = tx.prepare(
                    "SELECT r.l2_tx_count, r.batch_nonce, r.safe_input_index, r.sender, \
                            r.nonce, r.fee, r.safe_block, r.data, \
                            d.sender, d.block_number, d.block_timestamp, d.payload \
                     FROM rebuilt_history r \
                     LEFT JOIN safe_inputs d ON d.safe_input_index = r.safe_input_index \
                     ORDER BY r.l2_tx_count",
                )?;
                statement
                    .query_map([], |row| {
                        let tx = match row.get::<_, Option<i64>>(2)? {
                            Some(input_index) => L2Tx::Direct(Direct {
                                input_index: to_u64(input_index),
                                sender: Address::from_slice(&row.get::<_, Vec<u8>>(8)?),
                                block: to_u64(row.get(9)?),
                                timestamp: to_u64(row.get(10)?),
                                payload: row.get(11)?,
                            }),
                            None => L2Tx::UserOp(UserOpTx {
                                sender: Address::from_slice(&row.get::<_, Vec<u8>>(3)?),
                                nonce: u32::try_from(row.get::<_, i64>(4)?).unwrap(),
                                fee: u16::try_from(row.get::<_, i64>(5)?).unwrap(),
                                block: to_u64(row.get(6)?),
                                data: row.get(7)?,
                            }),
                        };
                        Ok((
                            L2TxCount::new(to_u64(row.get(0)?)),
                            Scheduled {
                                batch_nonce: to_u64(row.get(1)?),
                                tx,
                            },
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
    }

    /// What the reference does with `inputs[from..to]`: the transactions it
    /// executes, each with its count, and the user ops it skips.
    struct Expected {
        executed: Vec<(L2TxCount, Scheduled)>,
        skipped: Vec<Scheduled>,
    }

    fn expected(
        runner: &mut CanonicalRunner<WalletApp>,
        inputs: &[IngestedSafeInput],
        from: usize,
        to: usize,
    ) -> Expected {
        let mut expected = Expected {
            executed: Vec::new(),
            skipped: Vec::new(),
        };
        for (index, input) in (u64::try_from(from).unwrap()..).zip(&inputs[from..to]) {
            let advanced = runner.process_input(l1_input(index, input)).unwrap();
            for record in advanced.txs {
                match record.fate {
                    TxFate::Done { index } => expected.executed.push((index, record.scheduled)),
                    TxFate::Skipped(_) => expected.skipped.push(record.scheduled),
                }
            }
        }
        expected
    }

    /// The stored rows are the executed ones, in order, with their counts,
    /// batch nonces and contents; no skipped op is stored.
    fn assert_stored(storage: &mut Storage, expected: &Expected) {
        let stored = stored(storage);
        assert_eq!(stored, expected.executed);
        for skipped in &expected.skipped {
            assert!(
                stored.iter().all(|(_, row)| row != skipped),
                "a skipped op is stored: {skipped:?}"
            );
        }
    }

    #[test]
    fn genesis_rebuild_matches_the_canonical_runner_row_for_row() {
        let inputs = chain();
        let (_db, mut storage) = synced("rebuild-genesis", &inputs, 30);
        let through_stop = storage.safe_input_count_through_block(30).unwrap();
        assert_eq!(through_stop, 9);
        let mut runner = reference(&inputs, 0);
        let export = runner.export();

        check_export(&mut storage, &export, through_stop, 30, cap()).unwrap();
        let mut app = wallet();
        let state =
            rebuild_history(&mut storage, &mut app, config(), &export, through_stop, 30).unwrap();

        let expected = expected(&mut runner, &inputs, 0, 9);
        assert_stored(&mut storage, &expected);
        assert!(
            !expected.skipped.is_empty(),
            "the fixture holds a skipped op"
        );
        assert_eq!(state, runner.scheduler().state());
        assert_eq!(
            (state.first_queued_input, state.fridge_len),
            (6, 2),
            "the directs after the last frame stay queued"
        );
        assert_eq!(app.l2_tx_count(), runner.app().l2_tx_count());
        assert_eq!(
            app.canonical_snapshot_bytes().unwrap(),
            runner.app().canonical_snapshot_bytes().unwrap()
        );
    }

    /// The worked boundary, with the 1200-block wait bound. A malformed batch
    /// from the submitter consumes no batch nonce, yet the backstop runs
    /// before decoding and executes the overdue directs; the last frame
    /// drains the direct at its own safe block, so nothing stays queued.
    ///
    /// | L1 input | Block | Input | Executes |
    /// |---:|---:|---|---|
    /// | 0 | 5 | `D0` | queued |
    /// | 1 | 12 | `D1` | queued |
    /// | 2 | 20 | batch 0, safe block 10, `U0` | `D0`, `U0` |
    /// | 3 | 20 | `D2`, after the batch | queued |
    /// | 4 | 24 | `D3` | queued |
    /// | 5 | 1230 | malformed batch | the backstop: `D1`, `D2`, `D3` |
    /// | 6 | 1231 | `D4` | queued |
    /// | 7 | 1232 | batch 1, safe block 1231, `U1` | `D4`, `U1` |
    ///
    /// At stop block 1240 the era has 7 transactions, next batch nonce 2 and
    /// first queued input 8, the input count through the stop block.
    #[test]
    fn the_worked_boundary_rebuilds_seven_transactions_and_queues_nothing() {
        const STOP: u64 = 1240;
        let inputs = vec![
            direct(5, vec![1]),
            direct(12, vec![2]),
            batch(0, 20, 10, 0),
            direct(20, vec![3]),
            direct(24, vec![4]),
            IngestedSafeInput {
                sender: SUBMITTER,
                ..direct(1230, vec![0])
            },
            direct(1231, vec![5]),
            batch(1, 1232, 1231, 1),
        ];
        let (_db, mut storage) = synced("rebuild-worked-boundary", &inputs, STOP);
        let data_dir = tempfile::tempdir().unwrap();
        let summary = finish(&mut storage, STOP, STOP, (3, 3), data_dir.path()).unwrap();

        // Each row: its L2 tx index, and the direct's input index (`None`
        // for a user op). Every transaction of this trace executes.
        let rows: Vec<(u64, Option<u64>)> = stored(&mut storage)
            .into_iter()
            .map(|(count, scheduled)| {
                let direct = match scheduled.tx {
                    L2Tx::Direct(direct) => Some(direct.input_index),
                    L2Tx::UserOp(_) => None,
                };
                (count.get(), direct)
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (0, Some(0)),
                (1, None),
                (2, Some(1)),
                (3, Some(3)),
                (4, Some(4)),
                (5, Some(6)),
                (6, None),
            ]
        );
        assert_eq!(summary.inputs_to_replay, 8);
        assert_eq!(
            summary.era,
            Some(crate::storage::NewEra {
                base: L2TxCount::ZERO,
                rebuilt_end: L2TxCount::new(7),
                stop_block: STOP,
                first_queued_input: 8,
                anchor_batch_nonce: 2,
            })
        );
        assert_eq!(storage.era().unwrap().anchor_batch_nonce, 2);
        // The first live frame drains from the first input past the stop block.
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 8);

        let mut runner = reference(&inputs, 0);
        let expected = expected(&mut runner, &inputs, 0, 8);
        assert!(expected.skipped.is_empty());
        assert_stored(&mut storage, &expected);
    }

    /// A base in the middle with two directs queued: the rebuild resumes with
    /// them and lands where the reference over the whole range does.
    #[test]
    fn rebuild_from_a_base_with_queued_directs_matches_the_whole_range() {
        let inputs = chain();
        let (_db, mut storage) = synced("rebuild-middle", &inputs, 30);
        let mut runner = reference(&inputs, 5);
        let export = runner.export();
        assert_eq!((export.fridge_len, export.first_queued_input), (2, 1));

        check_export(&mut storage, &export, 9, 30, cap()).unwrap();
        let mut app = runner.app().clone();
        let state = rebuild_history(&mut storage, &mut app, config(), &export, 9, 30).unwrap();

        let expected = expected(&mut runner, &inputs, 5, 9);
        assert_stored(&mut storage, &expected);
        assert_eq!(state, runner.scheduler().state());
        assert_eq!(
            app.canonical_snapshot_bytes().unwrap(),
            runner.app().canonical_snapshot_bytes().unwrap()
        );
    }

    /// Inputs past the stop block are synced but never fed.
    #[test]
    fn rebuild_stops_at_the_stop_block_not_the_synced_head() {
        let inputs = chain();
        let (_db, mut storage) = synced("rebuild-stop", &inputs, 30);
        let through_stop = storage.safe_input_count_through_block(17).unwrap();
        assert_eq!(through_stop, 8);
        let mut runner = reference(&inputs, 0);
        let export = runner.export();

        let mut app = wallet();
        let state =
            rebuild_history(&mut storage, &mut app, config(), &export, through_stop, 17).unwrap();

        assert_stored(&mut storage, &expected(&mut runner, &inputs, 0, 8));
        assert_eq!(state, runner.scheduler().state());
        assert_eq!(app.l2_tx_count(), runner.app().l2_tx_count());
        assert_eq!(state.l1_input_count, 8);
    }

    #[test]
    fn export_check_refuses_a_disagreeing_queue() {
        let inputs = chain();
        let (_db, mut storage) = synced("export-check", &inputs, 30);
        let honest = reference(&inputs, 5).export();
        check_export(&mut storage, &honest, 9, 30, cap()).unwrap();

        let refused = |storage: &mut Storage, export: SchedulerExport| match check_export(
            storage,
            &export,
            9,
            30,
            cap(),
        ) {
            Err(CommandError::Bootstrap(crate::commands::BootstrapError::Setup(error))) => *error,
            other => panic!("expected a setup refusal, got {other:?}"),
        };
        // One queued direct too few.
        assert!(matches!(
            refused(
                &mut storage,
                SchedulerExport {
                    fridge_len: 1,
                    ..honest
                }
            ),
            SetupError::ExportDisagrees {
                admitted: 2,
                fridge_len: 1,
                first_admitted: Some(1),
                ..
            }
        ));
        // The right number, starting at the wrong input.
        assert!(matches!(
            refused(
                &mut storage,
                SchedulerExport {
                    first_queued_input: 4,
                    ..honest
                }
            ),
            SetupError::ExportDisagrees {
                first_queued_input: 4,
                first_admitted: Some(1),
                ..
            }
        ));
        // A clock past a queued direct: one fewer lies above it.
        assert!(matches!(
            refused(
                &mut storage,
                SchedulerExport {
                    clock: 11,
                    ..honest
                }
            ),
            SetupError::ExportDisagrees {
                admitted: 1,
                first_admitted: Some(4),
                ..
            }
        ));
        // Nothing queued while a direct lies above the clock.
        assert!(matches!(
            refused(
                &mut storage,
                SchedulerExport {
                    fridge_len: 0,
                    first_queued_input: 5,
                    ..honest
                }
            ),
            SetupError::ExportDisagrees { admitted: 2, .. }
        ));
        // A base past the stop block.
        assert!(matches!(
            match check_export(&mut storage, &honest, 4, 13, cap()) {
                Err(CommandError::Bootstrap(crate::commands::BootstrapError::Setup(error))) =>
                    *error,
                other => panic!("expected a setup refusal, got {other:?}"),
            },
            SetupError::BaseAheadOfStop {
                l1_input_count: 5,
                inputs_through_stop: 4,
                stop_block: 13
            }
        ));
    }

    /// The scheduler's own resume checks count as the export check. This
    /// export queues nothing and no admitted direct lies above its clock, so
    /// the table check passes; but it names the over-cap direct as its first
    /// queued input, which resume refuses.
    #[test]
    fn resume_refusal_is_an_export_refusal_and_stores_nothing() {
        let inputs = chain();
        let (_db, mut storage) = synced("resume-refused", &inputs, 30);
        let export = SchedulerExport {
            clock: 14,
            first_queued_input: 3,
            fridge_len: 0,
            ..reference(&inputs, 5).export()
        };
        check_export(&mut storage, &export, 9, 30, cap()).unwrap();
        let mut app = reference(&inputs, 5).app().clone();
        let error = rebuild_history(&mut storage, &mut app, config(), &export, 9, 30)
            .expect_err("a range that does not match the state is refused");
        assert!(
            matches!(
                &error,
                CommandError::Bootstrap(crate::commands::BootstrapError::Setup(error))
                    if matches!(
                        **error,
                        SetupError::ResumeRefused(
                            sequencer_core::scheduler::ResumeError::FirstQueuedNotADirect {
                                index: 3
                            }
                        )
                    )
            ),
            "{error:?}"
        );
        assert!(stored(&mut storage).is_empty());
    }

    /// An application that fails on its direct number `fail_at` (from 0).
    struct FailingApp {
        count: L2TxCount,
        nonce: u32,
        directs: u32,
        fail_at: u32,
    }

    impl Application for FailingApp {
        fn user_nonce(&self, _sender: Address) -> Result<u32, AppError> {
            Ok(self.nonce)
        }
        fn apply_user_op(&mut self, _op: &UserOpTx) -> Result<AppOutputs, AppError> {
            self.nonce += 1;
            self.count.advance();
            Ok(Vec::new())
        }
        fn apply_direct(&mut self, _direct: &Direct) -> Result<AppOutputs, AppError> {
            if self.directs == self.fail_at {
                return Err(AppError::Internal {
                    reason: "injected".into(),
                });
            }
            self.directs += 1;
            self.count.advance();
            Ok(Vec::new())
        }
        fn l2_tx_count(&self) -> L2TxCount {
            self.count
        }
        fn max_method_payload_bytes() -> usize {
            0
        }
        fn scheduler_limits() -> sequencer_core::application::SchedulerLimits {
            WalletApp::scheduler_limits()
        }
        fn admit_user_op(
            &self,
            _sender: Address,
            _op: &sequencer_core::user_op::UserOp,
            _fee: u16,
        ) -> Result<sequencer_core::application::Admission, AppError> {
            unreachable!("setup never admits")
        }
        fn from_dump(_prefix: &std::path::Path) -> Result<Self, AppError> {
            unreachable!("setup never loads a dump")
        }
        fn create_dump(&mut self, _prefix: &std::path::Path) -> Result<(), AppError> {
            unreachable!("a failed setup writes no dump")
        }
        fn state_file_in_dump(prefix: &std::path::Path) -> std::path::PathBuf {
            prefix.to_path_buf()
        }
        fn from_canonical_state(
            _state_file: &std::path::Path,
            _count: L2TxCount,
        ) -> Result<Self, AppError> {
            unreachable!("built directly")
        }
    }

    /// An error stops setup at the transaction, named by its input, its
    /// position among the transactions that input released, and what it is,
    /// since it has no row. Pages flushed before it stay.
    #[test]
    fn an_application_error_names_the_input_position_and_transaction() {
        let inputs = chain();
        let (_db, mut storage) = synced("execute-fails", &inputs, 30);
        let export = reference(&inputs, 0).export();
        let mut app = FailingApp {
            count: L2TxCount::ZERO,
            nonce: 0,
            directs: 0,
            fail_at: 2,
        };
        let error = rebuild_history(&mut storage, &mut app, config(), &export, 9, 30)
            .expect_err("the application fails");
        // The third direct is input 4, the second drained by the batch at
        // input 7, after input 1.
        let CommandError::Bootstrap(crate::commands::BootstrapError::Setup(error)) = error else {
            panic!("expected a setup refusal, got {error:?}");
        };
        assert!(
            matches!(
                &*error,
                SetupError::HistoryExecution {
                    l1_input_index: 7,
                    position: 1,
                    transaction,
                    ..
                } if transaction == "direct input 4"
            ),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("at input 7, position 1 (direct input 4): "),
            "{error}"
        );
        let counts: Vec<u64> = stored(&mut storage)
            .into_iter()
            .map(|(count, _)| count.get())
            .collect();
        assert_eq!(
            counts,
            vec![0, 1],
            "the first page is stored, the second not"
        );

        let op = L2Tx::UserOp(UserOpTx {
            sender: DEPOSITOR,
            nonce: 3,
            fee: 0,
            block: 0,
            data: Vec::new(),
        });
        assert_eq!(describe(&op), format!("user op of {DEPOSITOR} at nonce 3"));
    }

    /// The cross-check reads the stored rows: a drained direct with no row
    /// leaves the stored directs short of the input table's. No input makes the
    /// loop drop a direct, so this drives the check on a tampered table; every
    /// rebuild test above passes through the same check untampered.
    #[test]
    #[should_panic(
        expected = "the rebuild stored 2 directs, 2 of them admitted inputs in [0, 6), \
                    where 3 admitted inputs lie"
    )]
    fn a_drained_direct_without_a_row_trips_the_cross_check() {
        let inputs = chain();
        let (_db, mut storage) = synced("cross-check", &inputs, 30);
        let export = reference(&inputs, 0).export();
        let mut app = wallet();
        let end = rebuild_history(&mut storage, &mut app, config(), &export, 9, 30).unwrap();
        // Inputs 0, 1 and 4 drained; 2 is the submitter's and 3 is over the cap.
        assert_eq!(end.first_queued_input, 6);
        assert_drained_directs_stored(&mut storage, 0, &end, cap()).unwrap();
        storage
            .raw_write(|tx| {
                tx.execute_batch(
                    "DROP TRIGGER trg_rebuilt_history_not_deletable;
                     DELETE FROM rebuilt_history WHERE safe_input_index = 4;",
                )
            })
            .unwrap();
        let _ = assert_drained_directs_stored(&mut storage, 0, &end, cap());
    }

    /// A fridge at its bound drains whole blocks on arrival, and those
    /// directs execute too: the rebuild still matches the reference, and the
    /// cross-check counts them.
    #[test]
    fn an_overflowing_fridge_rebuilds_like_the_reference() {
        let inputs = chain();
        let (_db, mut storage) = synced("rebuild-overflow", &inputs, 30);
        let bounded = SchedulerConfig {
            fridge_bound: 1,
            ..config()
        };
        let mut runner = CanonicalRunner::genesis(wallet(), bounded);
        let export = runner.export();
        let mut app = wallet();
        let state = rebuild_history(&mut storage, &mut app, bounded, &export, 9, 30).unwrap();

        let expected = expected(&mut runner, &inputs, 0, 9);
        assert_stored(&mut storage, &expected);
        assert_eq!(state, runner.scheduler().state());
        assert_eq!(
            (state.first_queued_input, state.fridge_len),
            (8, 1),
            "every direct but the last drained"
        );
    }

    /// An export whose clock lies past the stop block passes the table check
    /// and resumes, but no scheduler at the stop block can hold that clock.
    #[test]
    fn an_export_clock_past_the_stop_block_is_refused() {
        let inputs = chain();
        let (_db, mut storage) = synced("clock-past-stop", &inputs, 30);
        let past = SchedulerExport {
            clock: 31,
            first_queued_input: 5,
            fridge_len: 0,
            ..reference(&inputs, 5).export()
        };
        check_export(&mut storage, &past, 9, 30, cap()).unwrap();
        let mut app = reference(&inputs, 5).app().clone();
        let error = rebuild_history(&mut storage, &mut app, config(), &past, 9, 30)
            .expect_err("the clock is past the stop block");
        assert!(
            matches!(
                &error,
                CommandError::Bootstrap(crate::commands::BootstrapError::Setup(error))
                    if matches!(**error, SetupError::ClockPastStop { clock: 31, stop_block: 30 })
            ),
            "{error:?}"
        );

        // At the stop block itself the clock is admissible.
        let (_db, mut storage) = synced("clock-at-stop", &inputs, 30);
        let at = SchedulerExport { clock: 30, ..past };
        let mut app = reference(&inputs, 5).app().clone();
        rebuild_history(&mut storage, &mut app, config(), &at, 9, 30).unwrap();
    }

    /// A batch whose frame drains every queued direct, then one more direct.
    fn drained_chain() -> Vec<IngestedSafeInput> {
        vec![
            direct(10, vec![1]),
            batch(0, 12, 11, 0),
            direct(13, vec![2]),
        ]
    }

    /// A base with nothing queued resumes over an empty range, and one at the
    /// stop block feeds nothing: the era rebuilds no row and ends at the base.
    #[test]
    fn a_base_with_an_empty_fridge_or_at_the_stop_block_rebuilds_nothing() {
        let inputs = drained_chain();
        let base = reference(&inputs, 2);
        let export = base.export();
        assert_eq!(
            (
                export.l1_input_count,
                export.first_queued_input,
                export.fridge_len
            ),
            (2, 2, 0)
        );

        // Past the base, a direct that only queues.
        let (_db, mut storage) = synced("empty-fridge-base", &inputs, 13);
        check_export(&mut storage, &export, 3, 13, cap()).unwrap();
        let mut app = base.app().clone();
        let state = rebuild_history(&mut storage, &mut app, config(), &export, 3, 13).unwrap();
        assert!(stored(&mut storage).is_empty());
        assert_eq!(state, reference(&inputs, 3).scheduler().state());

        // The base is the stop block's own state.
        let (_db, mut storage) = synced("base-at-stop", &inputs, 13);
        let dumps = tempfile::tempdir().unwrap();
        let summary = crate::commands::setup::finish_setup(
            &mut storage,
            crate::commands::setup::base::Base {
                export,
                app: base.app().clone(),
            },
            config(),
            crate::commands::setup::Synced {
                stop_block: 12,
                synced_safe_block: 13,
                wallet: (1, 1),
            },
            dumps.path(),
        )
        .unwrap();
        assert_eq!(summary.inputs_to_replay, 0);
        assert!(stored(&mut storage).is_empty());
        assert_eq!(
            summary.era,
            Some(crate::storage::NewEra {
                base: L2TxCount::new(export.l2_tx_count),
                rebuilt_end: L2TxCount::new(export.l2_tx_count),
                stop_block: 12,
                first_queued_input: 2,
                anchor_batch_nonce: 1,
            })
        );
    }

    fn genesis_base() -> crate::commands::setup::base::Base<WalletApp> {
        crate::commands::setup::base::Base {
            export: reference(&[], 0).export(),
            app: wallet(),
        }
    }

    fn finish(
        storage: &mut Storage,
        stop_block: u64,
        synced_safe_block: u64,
        wallet: (u64, u64),
        data_dir: &std::path::Path,
    ) -> Result<crate::commands::setup::SetupSummary, CommandError> {
        crate::commands::setup::finish_setup(
            storage,
            genesis_base(),
            config(),
            crate::commands::setup::Synced {
                stop_block,
                synced_safe_block,
                wallet,
            },
            data_dir,
        )
    }

    /// The sync overshoots the stop block; the era is the scheduler's at the
    /// stop block, never at the synced head.
    #[test]
    fn setup_publishes_the_era_at_the_stop_block_not_the_synced_head() {
        let inputs = chain();
        let (_db, mut storage) = synced("finish-overshoot", &inputs, 30);
        let data_dir = tempfile::tempdir().unwrap();
        let summary = finish(&mut storage, 17, 30, (3, 3), data_dir.path()).unwrap();

        let at_stop = reference(&inputs, 8);
        let state = at_stop.scheduler().state();
        assert_eq!(
            summary.era,
            Some(crate::storage::NewEra {
                base: L2TxCount::ZERO,
                rebuilt_end: at_stop.app().l2_tx_count(),
                stop_block: 17,
                first_queued_input: state.first_queued_input,
                anchor_batch_nonce: state.next_batch_nonce,
            })
        );
        assert_eq!(summary.inputs_to_replay, 8);
        let published = storage.era().unwrap();
        assert_eq!(published.stop_block, 17);
        assert_eq!(published.anchor_batch_nonce, state.next_batch_nonce);
        // No row comes from an input past the stop block.
        assert_stored(
            &mut storage,
            &expected(&mut reference(&inputs, 0), &inputs, 0, 8),
        );
    }

    /// Every refusal after the sync comes before the rebuild writes a row.
    #[test]
    fn setup_refuses_after_the_sync_before_rebuilding() {
        let inputs = chain();
        type Refusal = (&'static str, u64, (u64, u64), fn(&CommandError) -> bool);
        let refusals: [Refusal; 3] = [
            // The sync stopped below the stop block.
            ("finish-sync-behind", 31, (3, 3), |error| {
                matches!(
                    error,
                    CommandError::Bootstrap(crate::commands::BootstrapError::Setup(error))
                        if matches!(
                            **error,
                            SetupError::SyncBehindStopBlock {
                                synced_safe_block: 30,
                                stop_block: 31
                            }
                        )
                )
            }),
            // The batch at input 7 (block 17) lies past stop block 15.
            ("finish-escape-input", 15, (3, 3), |error| {
                matches!(
                    error,
                    CommandError::Bootstrap(crate::commands::BootstrapError::SetupRefuse(
                        crate::commands::error::SetupRefuse::SubmitterInputPastStop {
                            stop_block: 15,
                            found_block: 17,
                            safe_input_index: 7,
                        }
                    ))
                )
            }),
            ("finish-escape-wallet", 30, (4, 3), |error| {
                matches!(
                    error,
                    CommandError::Bootstrap(crate::commands::BootstrapError::SetupRefuse(
                        crate::commands::error::SetupRefuse::WalletUnsettledAfterFlush {
                            pending: 4,
                            safe: 3,
                        }
                    ))
                )
            }),
        ];
        for (name, stop_block, wallet, expected) in refusals {
            let (_db, mut storage) = synced(name, &inputs, 30);
            let data_dir = tempfile::tempdir().unwrap();
            let error =
                finish(&mut storage, stop_block, 30, wallet, data_dir.path()).expect_err(name);
            assert!(expected(&error), "{name}: {error:?}");
            if !name.starts_with("finish-sync") {
                assert_eq!(
                    error.exit_code(),
                    crate::commands::error::EXIT_SETUP_ESCAPE,
                    "{name}"
                );
            }
            assert!(!storage.is_setup_complete().unwrap(), "{name}");
            assert!(stored(&mut storage).is_empty(), "{name}");
            assert!(!data_dir.path().join("dumps").exists(), "{name}");
        }
    }

    /// The era's root frame drains what the canonical scheduler drains with
    /// the era's first frame: the directs queued at the stop block, skipping
    /// the submitter's input and the over-cap direct among them, then the
    /// directs up to the root frame's safe block, all in the anchor batch.
    #[test]
    fn the_root_frame_drains_what_the_scheduler_drains_with_the_first_frame() {
        let inputs = vec![
            direct(10, vec![1]),
            direct(11, vec![2]),
            batch(0, 12, 10, 0),
            direct(13, vec![0; usize::try_from(cap()).unwrap() + 1]),
            direct(14, vec![3]),
            direct(20, vec![4]),
        ];
        // The safe head is fresh, so run's danger check lets the root open.
        let db = temp_db("root-drain");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
        pin_test_deployment_identity(&mut storage, SUBMITTER);
        let now_ms = crate::clock::unix_now_ms();
        storage
            .append_ingested_safe_inputs_with_timestamp(
                30,
                now_ms / 1000,
                &inputs,
                &default_protocol_timing(),
            )
            .unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let era = finish(&mut storage, 14, 30, (1, 1), data_dir.path())
            .unwrap()
            .era
            .unwrap();
        assert_eq!((era.first_queued_input, era.anchor_batch_nonce), (1, 1));

        // Run's startup opens the root at its own safe head.
        storage
            .ensure_open_tip_for_recovery(&default_protocol_timing(), now_ms, cap())
            .unwrap();
        let root = storage.open_state().unwrap().expect("the root is open");
        assert_eq!(root.safe_block(), 30);
        let version = storage.history_version().unwrap();
        let drained: Vec<(u64, u64, u64)> = storage
            .canonical_history_page(
                sequencer_core::history::HistoryClaim {
                    version,
                    next_input: era.rebuilt_end,
                },
                16,
            )
            .unwrap()
            .rows
            .into_iter()
            .map(|row| match row.context {
                crate::storage::L2TxContext::Direct {
                    tx, batch_nonce, ..
                } => (row.offset.get(), tx.input_index, batch_nonce),
                other => panic!("the root frame drains only directs, got {other:?}"),
            })
            .collect();

        // The canonical scheduler at the stop block, then the inputs past it,
        // then the root's batch landing with its one frame.
        let mut runner = reference(&inputs, 5);
        let mut handed_out = Vec::new();
        let root_batch = IngestedSafeInput {
            sender: SUBMITTER,
            payload: Batch {
                nonce: era.anchor_batch_nonce,
                frames: vec![Frame {
                    safe_block: root.safe_block(),
                    fee_price: 0,
                    user_ops: Vec::new(),
                }],
            }
            .as_ssz_bytes(),
            block_number: 31,
            block_timestamp: 1_700_000_031,
            transaction_hash: B256::repeat_byte(0xcc),
        };
        for (index, input) in (5_u64..).zip(inputs[5..].iter().chain([&root_batch])) {
            for record in runner.process_input(l1_input(index, input)).unwrap().txs {
                let TxFate::Done { index: l2_tx_index } = record.fate else {
                    panic!("a direct never skips");
                };
                let L2Tx::Direct(direct) = record.scheduled.tx else {
                    panic!("the frame holds no op");
                };
                handed_out.push((
                    l2_tx_index.get(),
                    direct.input_index,
                    record.scheduled.batch_nonce,
                ));
            }
        }
        assert_eq!(drained, handed_out);
        assert_eq!(
            drained
                .iter()
                .map(|(_, input_index, _)| *input_index)
                .collect::<Vec<_>>(),
            vec![1, 4, 5]
        );
    }

    #[test]
    fn counts_the_submitters_inputs_in_a_range() {
        let inputs = chain();
        let (_db, mut storage) = synced("count-submitter", &inputs, 30);
        assert_eq!(
            count_submitter_inputs(&mut storage, SUBMITTER, 0, 9).unwrap(),
            3
        );
        assert_eq!(
            count_submitter_inputs(&mut storage, SUBMITTER, 3, 9).unwrap(),
            2
        );
        assert_eq!(
            count_submitter_inputs(&mut storage, SUBMITTER, 8, 9).unwrap(),
            0
        );
        assert_eq!(
            count_submitter_inputs(&mut storage, DEPOSITOR, 9, 9).unwrap(),
            0
        );
    }
}
