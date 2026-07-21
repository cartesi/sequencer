// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! First-snapshot DB fills for the `setup` subcommand: the two ways `setup`
//! writes the initial finalized snapshot a freshly-prepared DB needs before
//! `run` will boot.
//!
//! - [`register_genesis_finalized_snapshot`] — plain `setup` (phase A): the
//!   genesis app state at nonce 0 / cursor 0 / `B = 0`.
//! - [`fill_recovery_state`] — `setup --recovery`: the folded
//!   cockroach-recovered state `S'`, tree anchored at `N'`, cursor past the
//!   `<= C` directs already in `S'`.
//!
//! Both are setup-time, called once each from [`super::setup::setup`], and
//! never by a runtime worker — hence their own module, distinct from the
//! worker lifecycle in [`super::workers`].

use crate::ingress::inclusion_lane::dump_info::{
    self, CreateDumpDirError, create_dump_dir_with_info,
};
use crate::runtime::error::{RunError, SetupRecoveryError};
use sequencer_core::application::Application;

/// Register the genesis application state as the finalized snapshot. Called
/// once by `setup` (phase A). Idempotent: a re-run with a finalized snapshot
/// already present is a no-op, so the `initial_app` is dropped.
///
/// The genesis dir is unique-per-attempt so a crash between dump creation and
/// `insert_finalized_dump` doesn't wedge a stale directory on the next
/// `setup`. The genesis checkpoint is born finalized: resume nonce 0, replay
/// cursor 0, `B` = 0 (implicit-genesis inclusion block, matching the row).
pub(crate) fn register_genesis_finalized_snapshot<A: Application + 'static>(
    initial_app: A,
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<(), RunError> {
    if storage.finalized_dump()?.is_some() {
        return Ok(());
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let genesis_dir = dumps_dir.join(format!("genesis-{nanos}"));
    create_dump_dir_with_info(
        &initial_app,
        &genesis_dir,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce: 0,
            l2_tx_index: 0,
            promoted_inclusion_block: Some(0),
        },
    )
    .map_err(|err| match err {
        CreateDumpDirError::App(e) => RunError::from(e),
        CreateDumpDirError::Io(e) => RunError::from(e),
    })?;
    storage.insert_finalized_dump(&genesis_dir, 0, 0)?;
    Ok(())
}

/// Fill the freshly-wiped DB with the cockroach-recovered state, the recovery
/// analog of [`register_genesis_finalized_snapshot`]. Given
/// the folded application state `S'`, the resume batch nonce `N'`, and the
/// post-flush stopping block `C`, it leaves the DB in exactly the shape `run`'s
/// startup expects: a finalized snapshot of `S'`, a batch tree rooted at `N'`,
/// and a replay cursor past every `≤ C` direct (already executed inside `S'`).
///
/// Crash-before-marker re-entry is **fail-loud**, not blindly idempotent. Only a
/// *completed* fill (finalized snapshot present — the last write) re-runs as a
/// safe no-op. Any other existing root tip is a crashed mid-fill and is refused:
///   * a *different* `N'` (a different checkpoint, or the same one after `C`
///     advanced with more accepted `(B, C]` **batches**) would re-anchor the
///     tree while leaving the old root tip in place, silently breaking I16 —
///     [`SetupRecoveryError::PartialRecoveryMismatch`];
///   * the *same* `N'` with no finalized snapshot is a crash between step 2 and
///     step 4. A re-sync may have advanced `C` with new **directs** (which leave
///     `N'` unchanged) that resuming would leave unsequenced, so the snapshot
///     cursor lags `S'` and `run` would drain + execute them a second time —
///     [`SetupRecoveryError::PartialRecoveryIncomplete`].
///
/// In both cases the operator wipes and re-runs (the one-shot recovery model).
///
/// Order matters and differs from genesis: the root tip is opened *before* the
/// finalized snapshot, because the snapshot's `l2_tx_index` must equal the
/// global replay head *after* the tip's first frame has sequenced the `(*, C]`
/// directs. `run`'s catch-up replays `offset > l2_tx_index`, so those directs
/// (offsets below the head) are skipped — they are already in `S'`, while
/// on-chain `run`'s first batch (frame `safe_block ≥ C`) drains them once.
pub(crate) fn fill_recovery_state<A: Application + 'static>(
    recovered_app: A,
    resume_nonce: u64,
    // `C`, the post-flush stop block; recorded as the snapshot's
    // `promoted_inclusion_block` and the recovery tip frame's safe block.
    stop_block: u64,
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<(), RunError> {
    // Re-entry guard (rationale + the strict one-shot model are in the
    // docstring). The tip is opened before the snapshot, so an existing root tip
    // means a prior attempt: only a *completed* fill (finalized snapshot present)
    // is a safe no-op; anything else is refused fail-loud.
    if let Some(existing) = storage.open_tip_nonce()? {
        // Different N' → re-anchoring would orphan the old root tip (I16 break).
        if existing != resume_nonce {
            return Err(SetupRecoveryError::PartialRecoveryMismatch {
                existing_root_nonce: existing,
                requested_nonce: resume_nonce,
            }
            .into());
        }
        // Same N', no snapshot → crashed mid-fill; directs that landed since
        // would be left unsequenced (cursor lags `S'` → `run` double-drains).
        if storage.finalized_dump()?.is_none() {
            return Err(SetupRecoveryError::PartialRecoveryIncomplete {
                root_nonce: existing,
            }
            .into());
        }
        return Ok(());
    }
    // No tip. A fresh fill — or anchor-only residue from a crash before step 2 —
    // proceeds below (re-anchoring + opening the tip completes it). But a
    // finalized snapshot with *no* root tip is residue from a different
    // deployment mode: a plain `setup` that wrote the genesis snapshot and
    // crashed before its marker. A completed cockroach fill always has both
    // (caught above), so reaching here with a snapshot means recovery is running
    // over an un-wiped data dir — silently keeping it would mark setup complete
    // over genesis instead of the folded `(S', N')`. Refuse (same fail-loud
    // one-shot model as the partial-recovery guards above).
    if let Some(finalized) = storage.finalized_dump()? {
        return Err(SetupRecoveryError::RecoveryOverResidualSnapshot {
            existing_finalized_block: finalized.inclusion_block,
        }
        .into());
    }
    // 1. Anchor the tree at N' so the single parentless root carries it.
    storage.set_batch_tree_anchor(resume_nonce)?;
    // 2. Open the recovery root tip at N' (parentless, via the anchor), at frame
    //    safe block `C`, draining only the `≤ C` directs the fold folded into
    //    `S'` into its first frame's leading range — sequenced but not executed.
    //    This advances the next-undrained cursor past them so the resumed lane
    //    never re-leads them. Directs the resync pulled in past `C` (the resync
    //    runs to the live safe head `H1`, normally `> C`) stay undrained: `run`'s
    //    lane leads and executes them exactly once as the frontier advances
    //    `C → H1`. (Draining them here would skip them on catch-up while `S'`
    //    never executed them — a divergence.)
    storage.open_recovery_tip(stop_block)?;
    // 3. The finalized snapshot's replay cursor = the global valid replay head
    //    AFTER step 2's sequencing.
    let head = storage.valid_ordered_l2_tx_head()?;
    // 4. Register S' as the finalized snapshot at block C (file-first). Unique
    //    per attempt so a crash before the DB row leaves only a swept orphan.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let recovery_dir = dumps_dir.join(format!("recovery-{nanos}"));
    create_dump_dir_with_info(
        &recovered_app,
        &recovery_dir,
        &dump_info::DumpInfo::at_recovery(resume_nonce, head, stop_block),
    )
    .map_err(|err| match err {
        CreateDumpDirError::App(e) => RunError::from(e),
        CreateDumpDirError::Io(e) => RunError::from(e),
    })?;
    storage.insert_finalized_dump(&recovery_dir, stop_block, head)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress::inclusion_lane::{InclusionLane, InclusionLaneConfig, InclusionLaneError};
    use crate::runtime::shutdown::ShutdownSignal;
    use crate::runtime::test_support::SweepTestApp;
    use crate::storage::Storage;
    use crate::storage::test_helpers::temp_db;
    use alloy_primitives::{Address, U256};
    use app_core::application::{WalletApp, WalletConfig};
    use std::time::Duration;

    #[test]
    fn fill_recovery_state_roots_tree_at_n_prime_and_skips_pre_executed_directs() {
        use crate::storage::StoredSafeInput;
        use crate::storage::test_helpers::default_protocol_timing;

        let db = temp_db("fill-recovery");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");
        let submitter = alloy_primitives::Address::repeat_byte(0x99);
        let direct = alloy_primitives::Address::repeat_byte(0x22);
        let timing = default_protocol_timing();

        // Sync a safe head at C = 100 with three `≤ C` inputs: a batch (from the
        // submitter) and two directs. These stand in for the (A, C] stream the
        // fold already folded into S'.
        let inputs = vec![
            StoredSafeInput {
                sender: submitter,
                payload: vec![0x00],
                block_number: 10,
                ..Default::default()
            },
            StoredSafeInput {
                sender: direct,
                payload: vec![0xAA],
                block_number: 20,
                ..Default::default()
            },
            StoredSafeInput {
                sender: direct,
                payload: vec![0xBB],
                block_number: 30,
                ..Default::default()
            },
        ];
        storage
            .append_safe_inputs(100, &inputs, submitter, &timing)
            .expect("sync through C");

        // Fill at resume nonce N' = 3, C = 100.
        let n_prime = 3;
        fill_recovery_state(SweepTestApp, n_prime, 100, &mut storage, dumps_dir.path())
            .expect("recovery fill");

        // The tree is anchored at N' and the single root tip carries it.
        assert_eq!(storage.batch_tree_anchor().expect("anchor"), n_prime);
        let root_idx = storage
            .latest_batch_index()
            .expect("idx")
            .expect("a root tip exists");
        assert_eq!(
            storage.batch_nonce(root_idx).expect("nonce"),
            n_prime,
            "root tip roots at N' (via the anchor)"
        );

        // All three `≤ C` inputs are sequenced into the root frame, so the
        // next-undrained cursor sits past them — the resumed lane never re-leads
        // them, and they are already in S'.
        assert_eq!(
            storage.next_undrained_safe_input_index().expect("cursor"),
            3
        );

        // Finalized snapshot at C, replay cursor = the post-sequencing head, so
        // run's catch-up (offset > l2_tx_index) skips the pre-executed directs.
        let finalized = storage
            .finalized_dump()
            .expect("read")
            .expect("finalized snapshot exists");
        assert_eq!(finalized.inclusion_block, 100);
        assert_eq!(
            finalized.l2_tx_index, 3,
            "catch-up starts past the pre-executed (≤C) directs"
        );

        // Idempotent: a re-run (crash before the setup marker) is a no-op, not a
        // duplicate-insert error.
        fill_recovery_state(SweepTestApp, n_prime, 100, &mut storage, dumps_dir.path())
            .expect("re-run is idempotent");
        assert_eq!(storage.batch_tree_anchor().expect("anchor"), n_prime);
    }

    #[test]
    fn fill_recovery_state_leaves_post_c_directs_undrained() {
        // P1: the post-flush resync runs to the live safe head H1, normally > the
        // checkpoint stop block C. The fold folds only `<= C` into S'; directs in
        // (C, H1] must stay UNDRAINED so run leads + executes them exactly once.
        // Draining them here (the old behavior) would skip them on catch-up while
        // S' never executed them — a vanished deposit / divergence.
        use crate::storage::StoredSafeInput;
        use crate::storage::test_helpers::default_protocol_timing;

        let db = temp_db("fill-recovery-post-c");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");
        let submitter = alloy_primitives::Address::repeat_byte(0x99);
        let direct = alloy_primitives::Address::repeat_byte(0x22);
        let timing = default_protocol_timing();

        // C = 100; the resync reached H1 = 150. Three directs <= C, one at block
        // 120 in the (C, H1] window.
        let inputs = vec![
            StoredSafeInput {
                sender: direct,
                payload: vec![0x01],
                block_number: 10,
                ..Default::default()
            },
            StoredSafeInput {
                sender: direct,
                payload: vec![0x02],
                block_number: 20,
                ..Default::default()
            },
            StoredSafeInput {
                sender: direct,
                payload: vec![0x03],
                block_number: 30,
                ..Default::default()
            },
            StoredSafeInput {
                sender: direct,
                payload: vec![0x04],
                block_number: 120,
                ..Default::default()
            },
        ];
        storage
            .append_safe_inputs(150, &inputs, submitter, &timing)
            .expect("sync through H1");

        fill_recovery_state(SweepTestApp, 3, 100, &mut storage, dumps_dir.path()).expect("fill");

        // All four inputs are in safe_inputs ...
        assert_eq!(storage.safe_input_end_exclusive().expect("end"), 4);
        // ... but only the three <= C directs are sequenced; the block-120 direct
        // (index 3) stays undrained for run to lead + execute.
        assert_eq!(
            storage.next_undrained_safe_input_index().expect("cursor"),
            3,
            "the (C, H1] direct must remain undrained"
        );
        let finalized = storage
            .finalized_dump()
            .expect("read")
            .expect("finalized snapshot");
        assert_eq!(
            finalized.l2_tx_index, 3,
            "catch-up cursor stops at the <= C directs, not the (C, H1] one"
        );
    }

    #[test]
    fn fill_recovery_state_drain_boundary_is_inclusive_at_exactly_c() {
        // The (C, H1] split must be inclusive at exactly C: a direct at block C
        // is folded into S' and drained here; one at C+1 is not. Off-by-one in
        // either direction is the same loss/double-execution class as the (C, H1]
        // bug, from the boundary.
        use crate::storage::StoredSafeInput;
        use crate::storage::test_helpers::default_protocol_timing;

        let db = temp_db("fill-recovery-boundary");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");
        let submitter = alloy_primitives::Address::repeat_byte(0x99);
        let direct = alloy_primitives::Address::repeat_byte(0x22);
        let timing = default_protocol_timing();

        // C = 100. Directs straddling it exactly: block 99 (< C), block 100
        // (== C), block 101 (> C). Synced head H1 = 150.
        let inputs = vec![
            StoredSafeInput {
                sender: direct,
                payload: vec![0x01],
                block_number: 99,
                ..Default::default()
            },
            StoredSafeInput {
                sender: direct,
                payload: vec![0x02],
                block_number: 100,
                ..Default::default()
            },
            StoredSafeInput {
                sender: direct,
                payload: vec![0x03],
                block_number: 101,
                ..Default::default()
            },
        ];
        storage
            .append_safe_inputs(150, &inputs, submitter, &timing)
            .expect("sync through H1");

        fill_recovery_state(SweepTestApp, 0, 100, &mut storage, dumps_dir.path()).expect("fill");

        // Blocks 99 and 100 (<= C) are drained; block 101 (> C) is not. Inclusive
        // at exactly C: cursor sits at index 2, not 1 (would drop the ==C direct)
        // and not 3 (would over-drain the >C direct).
        assert_eq!(
            storage.next_undrained_safe_input_index().expect("cursor"),
            2,
            "the block-C direct must be drained; the block-C+1 direct must not"
        );
        let finalized = storage
            .finalized_dump()
            .expect("read")
            .expect("finalized snapshot");
        assert_eq!(finalized.l2_tx_index, 2);
    }

    #[test]
    fn fill_recovery_state_refuses_re_run_with_a_different_nonce() {
        // B1: a re-run with a *different* resume nonce (e.g. a different
        // checkpoint, or the same one after C advanced) would move the anchor
        // while leaving the old root tip — a silent I16 break. It must fail loud.
        use crate::storage::StoredSafeInput;
        use crate::storage::test_helpers::default_protocol_timing;

        let db = temp_db("fill-recovery-nonce-mismatch");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");
        let submitter = alloy_primitives::Address::repeat_byte(0x99);
        let timing = default_protocol_timing();
        let inputs = vec![StoredSafeInput {
            sender: alloy_primitives::Address::repeat_byte(0x22),
            payload: vec![0xAA],
            block_number: 10,
            ..Default::default()
        }];
        storage
            .append_safe_inputs(100, &inputs, submitter, &timing)
            .expect("sync");

        // First attempt fills at N' = 3 (opens a root tip carrying nonce 3).
        fill_recovery_state(SweepTestApp, 3, 100, &mut storage, dumps_dir.path())
            .expect("first fill");

        // Re-run with a DIFFERENT nonce (5): the existing root tip carries 3, so
        // the tip-nonce guard fires *before* the finalized short-circuit.
        let err = fill_recovery_state(SweepTestApp, 5, 100, &mut storage, dumps_dir.path())
            .expect_err("different-nonce re-run must fail loud");
        assert!(
            matches!(
                err,
                RunError::Bootstrap(crate::runtime::error::BootstrapError::SetupRecovery(
                    SetupRecoveryError::PartialRecoveryMismatch {
                        existing_root_nonce: 3,
                        requested_nonce: 5,
                    }
                ))
            ),
            "expected PartialRecoveryMismatch, got {err:?}"
        );
    }

    #[test]
    fn fill_recovery_state_refuses_incomplete_same_nonce_re_run() {
        // P1: a crash between opening the recovery root tip (fill step 2) and
        // writing the finalized snapshot (step 4) leaves "tip exists, no
        // finalized". A same-N' re-run must NOT resume — directs that landed
        // since would be left unsequenced, leaving the snapshot cursor behind
        // `S'` and double-draining them on `run`. Must fail loud.
        //
        // We reproduce the EXACT on-disk residue a crashed fill leaves by
        // running the real fill writes (anchor + `open_recovery_tip`) and
        // stopping before the snapshot — no process crash / test hook needed,
        // since the residue is fully determined by which committed storage
        // writes happened.
        use crate::storage::StoredSafeInput;
        use crate::storage::test_helpers::default_protocol_timing;

        let db = temp_db("fill-recovery-incomplete");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");
        let submitter = alloy_primitives::Address::repeat_byte(0x99);
        let direct = alloy_primitives::Address::repeat_byte(0x22);
        let timing = default_protocol_timing();

        // Sync a safe head with one `≤ C` direct, then reproduce a crashed
        // mid-fill at N' = 3 with the production writes: anchor set + recovery
        // tip opened at C = 100, but no finalized snapshot.
        storage
            .append_safe_inputs(
                100,
                &[StoredSafeInput {
                    sender: direct,
                    payload: vec![0xAA],
                    block_number: 20,
                    ..Default::default()
                }],
                submitter,
                &timing,
            )
            .expect("sync");
        storage.set_batch_tree_anchor(3).expect("anchor");
        storage
            .open_recovery_tip(100)
            .expect("open recovery root tip");
        assert!(
            storage.finalized_dump().expect("read").is_none(),
            "precondition: no finalized snapshot (crashed mid-fill)"
        );

        // A new direct lands during the retry gap (C advances; N' unchanged).
        storage
            .append_safe_inputs(
                130,
                &[StoredSafeInput {
                    sender: direct,
                    payload: vec![0xBB],
                    block_number: 120,
                    ..Default::default()
                }],
                submitter,
                &timing,
            )
            .expect("resync with a new direct");

        // Re-run at the SAME N' = 3 must refuse — resuming would leave the new
        // direct unsequenced → double-execution on `run`.
        let err = fill_recovery_state(SweepTestApp, 3, 130, &mut storage, dumps_dir.path())
            .expect_err("incomplete same-nonce re-run must fail loud");
        assert!(
            matches!(
                err,
                RunError::Bootstrap(crate::runtime::error::BootstrapError::SetupRecovery(
                    SetupRecoveryError::PartialRecoveryIncomplete { root_nonce: 3 }
                ))
            ),
            "expected PartialRecoveryIncomplete, got {err:?}"
        );
    }

    #[test]
    fn fill_recovery_state_refuses_over_residual_finalized_snapshot() {
        // P2: `setup --recovery` over an un-wiped data dir left by a plain
        // `setup` that wrote the genesis finalized snapshot and crashed before
        // its marker (finalized snapshot present, NO root tip). A completed
        // cockroach fill always has both, so this residue must fail loud —
        // silently keeping it would mark setup complete over genesis instead of
        // the folded `(S', N')`.
        let db = temp_db("fill-recovery-residue");
        let mut storage = Storage::open(db.path.as_str()).expect("open");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");

        // Plain-setup residue: genesis finalized snapshot, no root tip.
        register_genesis_finalized_snapshot(SweepTestApp, &mut storage, dumps_dir.path())
            .expect("genesis snapshot");
        assert!(
            storage.latest_batch_index().expect("idx").is_none(),
            "precondition: no root tip"
        );

        let err = fill_recovery_state(SweepTestApp, 3, 100, &mut storage, dumps_dir.path())
            .expect_err("recovery over residual finalized snapshot must fail loud");
        assert!(
            matches!(
                err,
                RunError::Bootstrap(crate::runtime::error::BootstrapError::SetupRecovery(
                    SetupRecoveryError::RecoveryOverResidualSnapshot {
                        existing_finalized_block: 0,
                    }
                ))
            ),
            "expected RecoveryOverResidualSnapshot, got {err:?}"
        );
    }

    /// The (C, H1] deposit-window property, end-to-end at the run side: a portal
    /// deposit that lands AFTER the flush fixed `C` but within the resync's reach
    /// (`H1 > C`) is left UNDRAINED by the recovery fill, then led + executed
    /// EXACTLY ONCE by the real inclusion lane as the safe frontier advances
    /// `C -> H1` — not lost (drained early / skipped on catch-up) and not
    /// double-credited (executed by both the recovery drain and the lane lead).
    ///
    /// This drives the production lane in-process against a recovered DB — no
    /// subprocess, no L1, no test hooks — and replaces the former subprocess
    /// e2e. The per-cap unit tests (`..._leaves_post_c_directs_undrained`,
    /// `..._drain_boundary_is_inclusive_at_exactly_c`, the fold-source bound)
    /// pin each piece; this is the only test that observes the *composition*
    /// crediting the deposit exactly once across the setup -> run handoff.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_then_lane_credits_post_c_deposit_exactly_once() {
        use crate::storage::StoredSafeInput;
        use crate::storage::test_helpers::default_protocol_timing;

        let db = temp_db("recovery-credited-once");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");
        let timing = default_protocol_timing();

        // The recovered checkpoint state S' is the real `WalletApp` (only it
        // exposes a queryable balance). Its config round-trips through the SSZ
        // dump, so the lane reloads a devnet-configured app.
        let cfg = WalletConfig::devnet();
        let portal = cfg.erc20_portal_address;
        let token = cfg.supported_erc20_token;
        let recovered = WalletApp::new(cfg);
        let submitter = Address::repeat_byte(0x99); // ingest classifier (not the portal)
        let pre_c_direct = Address::repeat_byte(0x22); // non-portal => decode None => inert
        let nested_sender = Address::repeat_byte(0x77); // fresh account, zero balance in S'
        let deposit_value = U256::from(2_000_000_u64);

        // C = 100; the resync reached H1 = 150. Three `<= C` directs (already
        // folded into S'), plus ONE portal USDC deposit at block 120 in the
        // (C, H1] window. Deposit payload mirrors `encode_erc20_deposit_payload`:
        // token || nested_sender || value_be32.
        let deposit_payload = {
            let mut p = Vec::with_capacity(20 + 20 + 32);
            p.extend_from_slice(token.as_slice());
            p.extend_from_slice(nested_sender.as_slice());
            p.extend_from_slice(deposit_value.to_be_bytes::<32>().as_slice());
            p
        };
        let inputs = vec![
            StoredSafeInput {
                sender: pre_c_direct,
                payload: vec![0x01],
                block_number: 10,
                ..Default::default()
            },
            StoredSafeInput {
                sender: pre_c_direct,
                payload: vec![0x02],
                block_number: 20,
                ..Default::default()
            },
            StoredSafeInput {
                sender: pre_c_direct,
                payload: vec![0x03],
                block_number: 30,
                ..Default::default()
            },
            StoredSafeInput {
                sender: portal,
                payload: deposit_payload,
                block_number: 120,
                ..Default::default()
            },
        ];
        {
            let mut storage = Storage::open(db.path.as_str()).expect("open");
            storage
                .append_safe_inputs(150, &inputs, submitter, &timing)
                .expect("sync to H1 = 150");
            // Fill at N' = 3, C = 100: drains only the three `<= C` directs into
            // the recovery root frame; the deposit (index 3) stays undrained.
            fill_recovery_state(recovered, 3, 100, &mut storage, dumps_dir.path())
                .expect("recovery fill");

            // Preconditions (the (C, H1] cap — covered by the cap tests; pinned
            // here so a regression that drained the deposit fails early).
            assert_eq!(
                storage.next_undrained_safe_input_index().expect("cursor"),
                3,
                "the (C, H1] deposit must be left UNDRAINED at index 3"
            );
            let finalized = storage
                .finalized_dump()
                .expect("read")
                .expect("finalized S' exists");
            assert_eq!(
                finalized.l2_tx_index, 3,
                "catch-up starts past the <= C directs"
            );
            let s_prime = WalletApp::from_dump(&dump_info::app_prefix(&finalized.dump.prefix))
                .expect("load S'");
            assert_eq!(
                s_prime.current_user_balance(nested_sender),
                U256::ZERO,
                "S' itself must NOT have credited the deposit"
            );
        }

        // Drive the REAL run-side inclusion lane against the recovered DB. The
        // frontier advance `C -> H1` is already staged (l1_safe_head = 150, the
        // recovery tip frame's safe block = 100), so the lane's first iteration
        // leads the deposit at index 3 and executes it once. `batch_submitter`
        // differs from the portal, so the deposit is not skipped as an own-batch
        // input; the short `max_batch_open` forces a batch-close snapshot.
        let storage = Storage::open(db.path.as_str()).expect("reopen for lane");
        let config = InclusionLaneConfig {
            batch_submitter_address: Address::repeat_byte(0xff),
            dumps_dir: dumps_dir.path().to_path_buf(),
            max_user_ops_per_chunk: 16,
            safe_input_buffer_capacity: 16,
            max_batch_open: Duration::from_millis(10),
            idle_poll_interval: Duration::from_millis(2),
            frontier_min_interval: Duration::ZERO,
        };
        let shutdown = ShutdownSignal::default();
        let (_tx, handle) =
            InclusionLane::<WalletApp>::start(128, shutdown.clone(), storage, config);

        // Observe via a SECOND storage handle (WAL); the lane owns its own. The
        // executed-deposit state is externalized only through the pending dump
        // the lane writes at batch close. Wait for it to reflect the credit.
        let credited = wait_until(Duration::from_secs(5), || {
            let mut s = Storage::open(db.path.as_str()).expect("open observer");
            match s.latest_pending_dump().expect("read pending") {
                Some(p) => {
                    WalletApp::from_dump(&dump_info::app_prefix(&p.dump.prefix))
                        .expect("load lane snapshot")
                        .current_user_balance(nested_sender)
                        == deposit_value
                }
                None => false,
            }
        })
        .await;
        assert!(
            credited,
            "the lane must lead + execute the (C, H1] deposit, crediting it once"
        );

        // Exactly once: the drain cursor advanced to 4 (one past the deposit), so
        // a restart would not re-lead it; and the balance is the deposit value
        // (== once; 0 would be lost, 2x would be double-credited).
        {
            let mut s = Storage::open(db.path.as_str()).expect("open observer");
            assert_eq!(
                s.next_undrained_safe_input_index().expect("cursor"),
                4,
                "the deposit's drain cursor must advance exactly one past it"
            );
            let dump = s
                .latest_pending_dump()
                .expect("pending")
                .expect("a post-deposit pending dump");
            let app =
                WalletApp::from_dump(&dump_info::app_prefix(&dump.dump.prefix)).expect("load");
            assert_eq!(
                app.current_user_balance(nested_sender),
                deposit_value,
                "credited EXACTLY once"
            );
        }

        shutdown_lane(&shutdown, handle).await;
    }

    async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let started = tokio::time::Instant::now();
        while started.elapsed() < timeout {
            if predicate() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        predicate()
    }

    async fn shutdown_lane(
        shutdown: &ShutdownSignal,
        handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
    ) {
        shutdown.request_shutdown();
        let joined = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("wait for lane shutdown");
        let result = joined.expect("join lane task");
        assert!(result.is_ok(), "lane should shut down cleanly: {result:?}");
    }
}
