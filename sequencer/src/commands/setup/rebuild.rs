// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Steps 4 to 6 of setup, over an input table synced through the stop block:
//! check the base's export against L1, rebuild the history with the scheduler
//! alone, then execute it. Every pass streams the tables in pages, so memory
//! stays bounded however long the rebuilt range is.

use alloy_primitives::Address;
use sequencer_core::application::{Application, Executed, execute};
use sequencer_core::scheduler::{
    L1Input, Scheduled, Scheduler, SchedulerConfig, SchedulerExport, SchedulerState,
};

use crate::commands::error::{CommandError, SetupError};
use crate::storage::{RebuiltKey, Storage};

/// Rows per page in every pass. Tests use a tiny page, so their few rows
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

/// Step 5: resume the scheduler from the export with the inputs it still had
/// queued, feed it every input it has not consumed through the stop block,
/// and store each transaction it hands out, in order and without a count.
/// Returns the scheduler's state at the end of the stop block.
pub(crate) fn rebuild_history(
    storage: &mut Storage,
    config: SchedulerConfig,
    export: &SchedulerExport,
    inputs_through_stop: u64,
    stop_block: u64,
) -> Result<SchedulerState, CommandError> {
    let mut scheduler = resume_from_export(storage, config, export)?;
    let state = export.scheduler_state();

    let mut rows: Vec<(u64, Scheduled)> = Vec::with_capacity(PAGE);
    let mut next = state.l1_input_count;
    while next < inputs_through_stop {
        let page = storage.l1_inputs_page(next, inputs_through_stop, PAGE)?;
        assert!(
            !page.is_empty(),
            "the input table ends at {next}, below the {inputs_through_stop} inputs it counted \
             through the stop block"
        );
        for input in page {
            let index = input.index;
            scheduler.feed(input);
            while let Some(scheduled) = scheduler.next_tx() {
                rows.push((index, scheduled));
                if rows.len() == PAGE {
                    storage.append_rebuilt_history(&rows)?;
                    rows.clear();
                }
            }
            next = index
                .checked_add(1)
                .expect("an input index has a successor");
        }
    }
    storage.append_rebuilt_history(&rows)?;

    let end = scheduler.state();
    check_clock(&end, stop_block)?;
    Ok(end)
}

/// Step 6: run the stored rows in order through the shared executor and
/// record each executed row's count; a skipped user op keeps none. An
/// application error stops setup at that row.
pub(crate) fn execute_history<A: Application>(
    storage: &mut Storage,
    app: &mut A,
) -> Result<(), CommandError> {
    let mut after: Option<RebuiltKey> = None;
    loop {
        let page = storage.rebuilt_history_after(after, PAGE)?;
        let Some(last) = page.last() else {
            return Ok(());
        };
        after = Some(last.key);
        let mut counts = Vec::with_capacity(page.len());
        for row in &page {
            match execute(app, &row.tx) {
                Ok(Executed::Done { index, .. }) => counts.push((row.key, index)),
                Ok(Executed::Skipped(_)) => {}
                Err(source) => {
                    return Err(SetupError::HistoryExecution {
                        l1_input_index: row.key.l1_input_index,
                        position: row.key.position,
                        source,
                    }
                    .into());
                }
            }
        }
        storage.record_rebuilt_counts(&counts)?;
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
    use crate::storage::{FrontierMode, IngestedSafeInput, LifecycleCommand};
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
                SUBMITTER,
                &default_protocol_timing(),
                FrontierMode::DeferUntilAnchorSet,
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

    /// Every stored row and its count, in order.
    fn stored(storage: &mut Storage) -> Vec<(u64, Scheduled, Option<L2TxCount>)> {
        let rows = storage.rebuilt_history_after(None, usize::MAX).unwrap();
        let counts: Vec<Option<i64>> = storage
            .read(|tx| {
                let mut statement = tx.prepare(
                    "SELECT l2_tx_count FROM rebuilt_history ORDER BY l1_input_index, position",
                )?;
                statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap();
        assert_eq!(rows.len(), counts.len());
        rows.into_iter()
            .zip(counts)
            .map(|(row, count)| {
                (
                    row.key.l1_input_index,
                    Scheduled {
                        batch_nonce: row.batch_nonce,
                        tx: row.tx,
                    },
                    count.map(|count| L2TxCount::new(u64::try_from(count).unwrap())),
                )
            })
            .collect()
    }

    /// What the reference hands out and executes for `inputs[from..to]`.
    fn expected(
        runner: &mut CanonicalRunner<WalletApp>,
        inputs: &[IngestedSafeInput],
        from: usize,
        to: usize,
    ) -> Vec<(u64, Scheduled, Option<L2TxCount>)> {
        let mut rows = Vec::new();
        for (index, input) in (u64::try_from(from).unwrap()..).zip(&inputs[from..to]) {
            let advanced = runner.process_input(l1_input(index, input)).unwrap();
            for record in advanced.txs {
                let count = match record.fate {
                    TxFate::Done { index: l2_tx_index } => Some(l2_tx_index),
                    TxFate::Skipped(_) => None,
                };
                rows.push((index, record.scheduled, count));
            }
        }
        rows
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
        let state = rebuild_history(&mut storage, config(), &export, through_stop, 30).unwrap();
        let mut app = wallet();
        execute_history(&mut storage, &mut app).unwrap();

        let expected = expected(&mut runner, &inputs, 0, 9);
        assert_eq!(stored(&mut storage), expected);
        assert!(
            expected.iter().any(|(_, _, count)| count.is_none()),
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
        let state = rebuild_history(&mut storage, config(), &export, 9, 30).unwrap();
        let mut app = runner.app().clone();
        execute_history(&mut storage, &mut app).unwrap();

        let expected = expected(&mut runner, &inputs, 5, 9);
        assert_eq!(stored(&mut storage), expected);
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

        let state = rebuild_history(&mut storage, config(), &export, through_stop, 17).unwrap();

        // Unexecuted, the rows carry no count yet.
        let expected: Vec<_> = expected(&mut runner, &inputs, 0, 8)
            .into_iter()
            .map(|(index, scheduled, _)| (index, scheduled, None))
            .collect();
        assert_eq!(stored(&mut storage), expected);
        assert_eq!(state, runner.scheduler().state());
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
        let error = rebuild_history(&mut storage, config(), &export, 9, 30)
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

    /// An application that fails on its second direct.
    struct FailingApp {
        count: L2TxCount,
        nonce: u32,
        directs: u32,
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
            if self.directs == 1 {
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

    /// The rows run in pages: an error stops setup at its row, the pages
    /// before it keep their counts, and its own page records none.
    #[test]
    fn an_application_error_names_the_row_and_keeps_earlier_counts() {
        let inputs = chain();
        let (_db, mut storage) = synced("execute-fails", &inputs, 30);
        let export = reference(&inputs, 0).export();
        rebuild_history(&mut storage, config(), &export, 9, 30).unwrap();

        let mut app = FailingApp {
            count: L2TxCount::ZERO,
            nonce: 0,
            directs: 0,
        };
        let error = execute_history(&mut storage, &mut app).expect_err("the application fails");
        // The second direct is input 1, drained first by the batch at input 7.
        let CommandError::Bootstrap(crate::commands::BootstrapError::Setup(error)) = error else {
            panic!("expected a setup refusal, got {error:?}");
        };
        assert!(
            matches!(
                *error,
                SetupError::HistoryExecution {
                    l1_input_index: 7,
                    position: 0,
                    ..
                }
            ),
            "{error:?}"
        );
        let counts: Vec<(u64, Option<u64>)> = stored(&mut storage)
            .into_iter()
            .map(|(index, _, count)| (index, count.map(L2TxCount::get)))
            .collect();
        assert_eq!(
            counts,
            vec![(2, Some(0)), (2, Some(1)), (7, None), (7, None), (7, None)],
            "the first page keeps its counts; the failed page records none"
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
        let error = rebuild_history(&mut storage, config(), &past, 9, 30)
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
        rebuild_history(&mut storage, config(), &at, 9, 30).unwrap();
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
        let state = rebuild_history(&mut storage, config(), &export, 3, 13).unwrap();
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
            Some(crate::storage::EraRecord {
                base_count: L2TxCount::new(export.l2_tx_count),
                rebuilt_history_end: L2TxCount::new(export.l2_tx_count),
                stop_block: 12,
                first_queued_input: 2,
                next_batch_nonce: 1,
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
            Some(crate::storage::EraRecord {
                base_count: L2TxCount::ZERO,
                rebuilt_history_end: at_stop.app().l2_tx_count(),
                stop_block: 17,
                first_queued_input: state.first_queued_input,
                next_batch_nonce: state.next_batch_nonce,
            })
        );
        assert_eq!(summary.inputs_to_replay, 8);
        assert_eq!(storage.history_state().unwrap().base_safe_block, 17);
        assert_eq!(storage.batch_tree_anchor().unwrap(), state.next_batch_nonce);
        assert!(
            stored(&mut storage).iter().all(|(index, _, _)| *index < 8),
            "no row comes from an input past the stop block"
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
                SUBMITTER,
                &default_protocol_timing(),
                FrontierMode::DeferUntilAnchorSet,
            )
            .unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let era = finish(&mut storage, 14, 30, (1, 1), data_dir.path())
            .unwrap()
            .era
            .unwrap();
        assert_eq!((era.first_queued_input, era.next_batch_nonce), (1, 1));

        // Run's startup opens the root at its own safe head.
        storage
            .ensure_open_tip_for_recovery(&default_protocol_timing(), now_ms, cap())
            .unwrap();
        let root = storage.open_state().unwrap().expect("the root is open");
        assert_eq!(root.safe_block, 30);
        let version = storage.history_state().unwrap().version;
        let drained: Vec<(u64, u64, u64)> = storage
            .canonical_history_page(
                sequencer_core::history::HistoryClaim {
                    version,
                    next_input: era.rebuilt_history_end,
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
                nonce: era.next_batch_nonce,
                frames: vec![Frame {
                    safe_block: root.safe_block,
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
