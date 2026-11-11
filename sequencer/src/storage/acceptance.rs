// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch acceptance: `batches.accepted_input`, the prefix of our batches
//! the on-chain scheduler accepted, simulated with `protocol::scheduler_accepts`
//! under the pinned window.
//! Its only writer is [`record_accepted_batches`], inside the input reader's
//! append transaction; a divergence marker freezes it (I9, I15).

use rusqlite::{Connection, OptionalExtension, Result, params};

use super::convert::{i64_to_u64, now_unix_ms, u64_to_i64};
use super::history::{era_in, find_era_in};
use super::l1_inputs::query_deployment_identity;
use sequencer_core::protocol::{AcceptedBatch, SafeInputView, scheduler_accepts};

/// The safe input of the greatest acceptance, or `None` before any.
fn latest_accepted_input(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row("SELECT MAX(accepted_input) FROM batches", [], |row| {
        row.get(0)
    })
}

/// Next nonce the scheduler is expected to accept — the gold frontier's
/// "expected next" cursor.
///
/// The greatest accepted batch's nonce + 1, else the era's anchor
/// (`anchor_batch_nonce`, its next batch nonce at the stop block). By the
/// contiguity of batch nonces this is the nonce of the first unaccepted
/// closed batch, the one the void history judges, when one exists (I16).
pub(super) fn frontier_nonce(conn: &Connection) -> Result<u64> {
    let latest: Option<i64> = conn
        .query_row(
            "SELECT nonce FROM batches \
             WHERE accepted_input = (SELECT MAX(accepted_input) FROM batches)",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match latest {
        Some(nonce) => Ok(next_expected_nonce(i64_to_u64(nonce))),
        // Before any acceptance the frontier sits at the anchor, the same
        // value `record_accepted_batches` seeds `expected` from: otherwise a
        // recovered deployment's submitter would fold from 0 and re-submit its
        // first batch every tick until the first acceptance (I16).
        None => Ok(era_in(conn)?.anchor_batch_nonce),
    }
}

fn next_expected_nonce(nonce: u64) -> u64 {
    nonce
        .checked_add(1)
        .expect("accepted batch nonce overflow: contract-impossible")
}

/// Mirror scheduler acceptance over new batch-submitter inputs onto
/// `batches.accepted_input`; an accepted landing that is foreign or
/// byte-different records `canonical_divergence` instead (I9, I15). The only
/// persistent cursor is the greatest accepted input: rows rejected at the
/// expected nonce are rescanned until a later acceptance. The expected-nonce
/// advance stays inline because it interleaves with the content-identity
/// check and the divergence freeze. Runs exactly when the era exists; the
/// submitter and the staleness window are the pinned deployment identity's.
pub(super) fn record_accepted_batches(conn: &Connection) -> Result<()> {
    const PAGE_SIZE: i64 = 256;
    const SELECT_SQL: &str = "SELECT safe_input_index, payload, block_number, transaction_hash \
                              FROM safe_inputs \
                              WHERE sender = ?1 AND safe_input_index > ?2 \
                                AND block_number > ?3 \
                              ORDER BY safe_input_index ASC LIMIT ?4";
    // The content-identity check found this closed batch; the schema
    // refuses an acceptance that does not extend the accepted prefix.
    const ACCEPT_SQL: &str = "UPDATE batches SET accepted_input = ?1 \
                              WHERE nonce = ?2 AND sealed_at_ms IS NOT NULL";

    // A persisted divergence marker freezes the acceptance frontier (I15).
    if canonical_divergence_in(conn)?.is_some() {
        return Ok(());
    }

    // The prefix through the stop block is the baseline's. Replaying it from
    // the anchor could accept an old future-nonce submission the scheduler
    // rejected at its original position. Only inputs after it belong here.
    let Some(era) = find_era_in(conn)? else {
        // Setup's sync, before the era exists: its stop block and anchor are
        // not known yet, and judging the old instance's landings against an
        // empty tree would record a false divergence (I15).
        return Ok(());
    };
    let stop_block = u64_to_i64(era.stop_block);
    let identity = query_deployment_identity(conn)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let batch_submitter = identity.batch_submitter_address;
    let max_wait_blocks = identity.max_wait_blocks;
    let mut cursor = latest_accepted_input(conn)?.unwrap_or(-1);
    let mut expected = frontier_nonce(conn)?;

    loop {
        // Materialize one page before executing any UPDATEs. rusqlite's row
        // iterator borrows the prepared statement, so we can't write on the
        // same connection while iterating. Once the page is collected and the
        // statement is dropped, the connection is free for writes.
        let page: Vec<(i64, Vec<u8>, i64, Vec<u8>)> = {
            let mut stmt = conn.prepare_cached(SELECT_SQL)?;
            stmt.query_map(
                params![batch_submitter.as_slice(), cursor, stop_block, PAGE_SIZE],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?
            .collect::<Result<_>>()?
        };

        if page.is_empty() {
            break;
        }
        let page_len = page.len() as i64;

        let mut accept_stmt = conn.prepare_cached(ACCEPT_SQL)?;
        for (safe_input_index, payload, block_number, transaction_hash) in &page {
            cursor = *safe_input_index;
            let input = SafeInputView {
                safe_input_index: i64_to_u64(*safe_input_index),
                sender: batch_submitter,
                payload: payload.as_slice(),
                inclusion_block: i64_to_u64(*block_number),
            };
            let Some(accepted) =
                scheduler_accepts(batch_submitter, max_wait_blocks, input, expected)
            else {
                continue;
            };

            // Content-identity check, gated on full acceptance —
            // exactly here, where the simulated scheduler accepted the
            // landing. Rejected/stale/undecodable copies are scheduler
            // no-ops; their content is irrelevant by construction.
            if let Some((kind, our_payload_hash)) =
                content_identity_violation(conn, &accepted, payload.as_slice())?
            {
                record_canonical_divergence_in(conn, &accepted, kind)?;
                tracing::error!(
                    nonce = accepted.nonce,
                    safe_input_index = accepted.safe_input_index,
                    kind = kind.as_str(),
                    transaction_hash = %alloy_primitives::hex::encode_prefixed(transaction_hash),
                    landed_payload_hash = %alloy_primitives::keccak256(payload),
                    our_payload_hash = %our_payload_hash
                        .map_or_else(|| "none".to_owned(), alloy_primitives::hex::encode_prefixed),
                    "CANONICAL DIVERGENCE: accepted L1 landing does not match \
                     our batch at this nonce; freezing the acceptance frontier \
                     — remedy is cockroach recovery (wipe + rebuild from L1)"
                );
                // Stop scanning; the marker (committed with this sync)
                // freezes the frontier and routes every subsequent boot and
                // landing-watch tick to the terminal stop, and the lane's next time-gated
                // frontier read (`Storage::safe_frontier_state`) returns
                // `CanonicalDivergence` instead of an open frontier (I15).
                return Ok(());
            }

            let changed = accept_stmt.execute(params![
                u64_to_i64(accepted.safe_input_index),
                u64_to_i64(accepted.nonce),
            ])?;
            if changed != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed));
            }
            expected = next_expected_nonce(expected);
        }

        if page_len < PAGE_SIZE {
            break;
        }
    }

    Ok(())
}

/// How an accepted landing failed the content-identity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DivergenceKind {
    /// No closed local batch exists at the accepted nonce — we never
    /// submitted one, so the landing is a zombie or foreign batch from our
    /// key.
    Foreign,
    /// A closed local batch exists, but the landed wire bytes hash
    /// differently from the bytes we sealed.
    Mismatch,
}

impl DivergenceKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Foreign => "foreign",
            Self::Mismatch => "mismatch",
        }
    }
}

/// Content-identity check proper: compare a fully-accepted landing against
/// our closed batch at the same nonce. `None` = identical (the normal
/// case); a mismatch also returns our payload hash.
///
/// Why content (not identity) suffices: accepted content-equal copies are
/// effect-equal — which physical tx landed carries no semantic weight. The
/// hash on our side was stamped at seal by the same encode path the
/// submitter broadcasts, so the comparison survives wire-format upgrades.
fn content_identity_violation(
    conn: &Connection,
    accepted: &AcceptedBatch,
    landed_payload: &[u8],
) -> Result<Option<(DivergenceKind, Option<Vec<u8>>)>> {
    let ours: Option<Option<Vec<u8>>> = conn
        .query_row(
            "SELECT payload_hash FROM closed_batches WHERE nonce = ?1",
            params![u64_to_i64(accepted.nonce)],
            |row| row.get(0),
        )
        .optional()?;
    match ours {
        None => Ok(Some((DivergenceKind::Foreign, None))),
        Some(None) => {
            // Sealed batches always carry a hash (stamped in the seal
            // UPDATE); a NULL here is an impossible state — fail loud.
            // Surface it as a structured error (rolls back the reader's sync
            // transaction and travels its normal error path) rather than a
            // panic that would crash the reader task mid-transaction.
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(format!(
                    "closed batch at nonce {} has no payload_hash",
                    accepted.nonce
                )),
            ))
        }
        Some(Some(hash)) => {
            let landed = alloy_primitives::keccak256(landed_payload);
            if hash.as_slice() == landed.as_slice() {
                Ok(None)
            } else {
                Ok(Some((DivergenceKind::Mismatch, Some(hash))))
            }
        }
    }
}

/// The persisted canonical-divergence marker (I15): the diverged landing's
/// batch nonce and the safe input that carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    pub nonce: u64,
    pub safe_input_index: u64,
}

/// The persisted divergence marker, if any.
pub(super) fn canonical_divergence_in(conn: &Connection) -> Result<Option<Divergence>> {
    conn.query_row(
        "SELECT nonce, safe_input_index FROM canonical_divergence WHERE singleton_id = 0",
        [],
        |row| {
            Ok(Divergence {
                nonce: i64_to_u64(row.get::<_, i64>(0)?),
                safe_input_index: i64_to_u64(row.get::<_, i64>(1)?),
            })
        },
    )
    .optional()
}

/// Persist the poison marker, atomically with the sync that detected it
/// (same transaction). Keep-first by construction: the frontier freezes at
/// the first detection, so this can only ever run once per marker lifetime
/// (the guard at the top of `record_accepted_batches`); a second
/// INSERT would fail loud on the singleton PK.
fn record_canonical_divergence_in(
    conn: &Connection,
    accepted: &AcceptedBatch,
    kind: DivergenceKind,
) -> Result<()> {
    conn.execute(
        "INSERT INTO canonical_divergence \
         (singleton_id, nonce, safe_input_index, kind, detected_at_ms) \
         VALUES (0, ?1, ?2, ?3, ?4)",
        params![
            u64_to_i64(accepted.nonce),
            u64_to_i64(accepted.safe_input_index),
            kind.as_str(),
            now_unix_ms(),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window the test identity pins (the wallet's).
    const WINDOW: u64 = app_core::application::WALLET_SCHEDULER_LIMITS.max_wait_blocks;
    use crate::storage::test_helpers::{
        SENDER_A, SENDER_B, local_batch_payload, seed_safe_inputs_with_batch_nonces, set_up_storage,
    };
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
    use crate::test_support::temp_db;
    use alloy_primitives::Address;

    #[test]
    fn recovered_prefix_does_not_reinterpret_a_rejected_future_nonce() {
        use crate::storage::test_helpers::local_batch_payload;
        use crate::storage::{LifecycleCommand, StoredSafeInput};
        use crate::test_support::pin_test_deployment_identity;
        use sequencer_core::{batch::Batch, history::L2TxCount};
        use ssz::Encode;

        // At C itself, the future nonce must precede nonce 0 in L1 order:
        // it was rejected there and must not become accepted after the rebuild.
        for old_future_block in [20, 30] {
            let db = temp_db("accepted-recovered-prefix");
            let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup)
                .expect("initialize setup");
            let submitter = Address::repeat_byte(0x99);
            pin_test_deployment_identity(&mut storage, submitter);
            let old_future = Batch {
                nonce: 1,
                frames: vec![],
            }
            .as_ssz_bytes();
            let old_accepted = Batch {
                nonce: 0,
                frames: vec![],
            }
            .as_ssz_bytes();
            assert!(
                scheduler_accepts(
                    submitter,
                    WINDOW,
                    SafeInputView {
                        safe_input_index: 0,
                        sender: submitter,
                        payload: &old_future,
                        inclusion_block: old_future_block,
                    },
                    0
                )
                .is_none()
            );
            assert!(
                scheduler_accepts(
                    submitter,
                    WINDOW,
                    SafeInputView {
                        safe_input_index: 1,
                        sender: submitter,
                        payload: &old_accepted,
                        inclusion_block: 30,
                    },
                    0
                )
                .is_some()
            );
            storage
                .append_safe_inputs_with_timestamp(
                    30,
                    30,
                    &[
                        StoredSafeInput {
                            sender: submitter,
                            payload: old_future,
                            block_number: old_future_block,
                        },
                        StoredSafeInput {
                            sender: submitter,
                            payload: old_accepted,
                            block_number: 30,
                        },
                    ],
                    submitter,
                )
                .expect("ingest opaque prefix");
            storage
                .write(|tx| {
                    super::super::history::insert_era_in(
                        tx,
                        &crate::storage::NewEra {
                            base: L2TxCount::new(41),
                            rebuilt_end: L2TxCount::new(41),
                            stop_block: 30,
                            first_queued_input: 2,
                            anchor_batch_nonce: 1,
                        },
                    )
                })
                .expect("install recovered baseline");
            storage.ensure_open_tip().expect("run opens the root");
            let mut head = storage.open_state().expect("read root").expect("root");
            storage
                .close_frame_and_batch(&mut head)
                .expect("close resumed batch");
            let payload = local_batch_payload(&mut storage, 1);
            storage
                .append_safe_inputs(
                    31,
                    &[StoredSafeInput {
                        sender: submitter,
                        payload,
                        block_number: 31,
                    }],
                    submitter,
                )
                .expect("accept post-baseline batch");
            assert!(
                storage
                    .canonical_divergence()
                    .expect("divergence")
                    .is_none(),
                "reinterpreted future-nonce input at block {old_future_block}"
            );
            let accepted: (i64, i64, i64) = storage
                .conn
                .query_row(
                    "SELECT COUNT(*), MAX(accepted_input), MAX(nonce) FROM batches \
                     WHERE accepted_input IS NOT NULL",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read acceptances");
            assert_eq!(accepted, (1, 2, 1), "exactly one acceptance");
        }
    }

    #[test]
    #[should_panic(expected = "accepted batch nonce overflow: contract-impossible")]
    fn accepted_batch_nonce_advance_fails_loud_on_overflow() {
        let _ = next_expected_nonce(u64::MAX);
    }
    #[test]
    fn record_accepted_batches_resumes_from_latest_row() {
        let db = temp_db("safe-accepted-frontier-resume");
        let mut storage = set_up_storage(&db.path, SENDER_A);

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for nonce in 0..4 {
            storage
                .close_frame_and_batch(&mut head)
                .unwrap_or_else(|e| panic!("close {nonce}: {e}"));
        }
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 10, &[0, 1]);

        // Mixed-sender wave: the SENDER_B row must be ignored, SENDER_A rows
        // must resume from the cursor and advance the frontier.
        let second_wave = vec![
            StoredSafeInput {
                sender: SENDER_B,
                payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                    nonce: 99,
                    frames: Vec::new(),
                }),
                block_number: 11,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: local_batch_payload(&mut storage, 2),
                block_number: 11,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: local_batch_payload(&mut storage, 3),
                block_number: 11,
            },
        ];
        storage
            .append_safe_inputs(11, second_wave.as_slice(), SENDER_A)
            .expect("append second wave");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.safe_block, 11);
        assert_eq!(frontier.accepted_next_nonce, 4);

        let accepted_count: i64 = storage
            .conn
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE accepted_input IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("count accepted rows");
        assert_eq!(accepted_count, 4);
    }

    /// Acceptance judges staleness by the pinned window, not the wallet's:
    /// under a pinned window of 50, a batch landing `s + 49` is accepted and
    /// one landing `s + 50` is not.
    #[test]
    fn acceptance_uses_the_pinned_window() {
        let db = temp_db("accepted-pinned-window");
        let mut storage =
            crate::storage::test_helpers::set_up_storage_with_window(&db.path, SENDER_A, 50);
        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.close_frame_and_batch(&mut head).expect("close 0");
        storage.close_frame_and_batch(&mut head).expect("close 1");
        let first = local_batch_payload(&mut storage, 0);
        let second = local_batch_payload(&mut storage, 1);
        storage
            .append_safe_inputs(
                150,
                &[
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: first,
                        block_number: 149,
                    },
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: second,
                        block_number: 150,
                    },
                ],
                SENDER_A,
            )
            .expect("append");
        assert_eq!(
            storage
                .submitter_frontier()
                .expect("frontier")
                .accepted_next_nonce,
            1,
            "batch 0 at s + 49 is accepted; batch 1 at s + 50 is stale"
        );
        assert_eq!(storage.canonical_divergence().expect("marker"), None);
    }

    #[test]
    fn accepted_frontier_skips_stale_payloads() {
        let db = temp_db("safe-accepted-frontier-skip-stale");
        let mut storage = set_up_storage(&db.path, SENDER_A);

        // Local batch 0 (first frame safe_block=1900) and batch 1 (first frame
        // safe_block=1900). Their landings carry the real wire bytes so the
        // content-identity check accepts them.
        let mut head = storage
            .initialize_open_state(1900, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.close_frame_and_batch(&mut head).expect("close 0");
        storage.close_frame_and_batch(&mut head).expect("close 1");

        // Non-stale landing of batch 0 (block 1950 - safe_block 1900 < 1200).
        let non_stale_payload = local_batch_payload(&mut storage, 0);
        // Stale copy at nonce 1 (safe_block=100, block 2000 → age >= 1200):
        // a scheduler no-op, so its content is never compared — synthetic
        // bytes are deliberate here.
        let stale_payload = super::super::test_helpers::make_stale_batch_payload(1, 100);
        // Non-stale landing of batch 1 (block 2000 - safe_block 1900 < 1200).
        let non_stale_payload_2 = local_batch_payload(&mut storage, 1);

        let inputs = vec![
            StoredSafeInput {
                sender: SENDER_A,
                payload: non_stale_payload,
                block_number: 1950,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: stale_payload,
                block_number: 2000,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: non_stale_payload_2,
                block_number: 2000,
            },
        ];
        storage
            .append_safe_inputs(2000, inputs.as_slice(), SENDER_A)
            .expect("append");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.accepted_next_nonce, 2);
    }

    #[test]
    fn seal_stamps_payload_hash_of_the_submitter_encode_path() {
        // Hash-at-seal: the hash stamped on the sealed row must
        // be the keccak256 of exactly the bytes the submitter will broadcast
        // (`pending_batches`'s encoding) — same code path, by construction.
        let db = temp_db("seal-stamps-payload-hash");
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head)
            .expect("close batch 0");

        let stamped: Vec<u8> = storage
            .conn
            .query_row(
                "SELECT payload_hash FROM batches WHERE nonce = 0",
                [],
                |row| row.get(0),
            )
            .expect("sealed batch must carry a payload hash");
        let wire = local_batch_payload(&mut storage, 0);
        assert_eq!(
            stamped.as_slice(),
            alloy_primitives::keccak256(&wire).as_slice(),
            "seal-time hash must match the submitter's wire bytes"
        );
    }

    #[test]
    fn accepted_landing_with_mismatched_content_freezes_frontier() {
        // The zombie / re-seal shape: a landing at the expected nonce
        // whose bytes differ from the batch we sealed. The check records a
        // 'mismatch' marker atomically with the sync and freezes the
        // frontier; later syncs stay frozen.
        let db = temp_db("mismatch-divergence");
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head)
            .expect("close batch 0");

        // Same nonce + same first-frame safe block (fresh by inclusion),
        // different content (synthetic fee 0 vs the sealed fee).
        let imposter = super::super::test_helpers::make_stale_batch_payload(0, 10);
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: imposter,
                    block_number: 20,
                }],
                SENDER_A,
            )
            .expect("sync commits; the marker rides the same transaction");

        let kind: String = storage
            .conn
            .query_row(
                "SELECT kind FROM canonical_divergence WHERE singleton_id = 0",
                [],
                |row| row.get(0),
            )
            .expect("marker persisted");
        assert_eq!(kind, "mismatch");
        let frontier = storage.submitter_frontier().expect("frontier");
        assert_eq!(frontier.accepted_next_nonce, 0, "frontier frozen");

        // A later sync — even one carrying the *correct* bytes — must not
        // thaw the frontier: the marker is terminal until cockroach recovery.
        let genuine = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                21,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: genuine,
                    block_number: 21,
                }],
                SENDER_A,
            )
            .expect("append after marker");
        let frontier = storage.submitter_frontier().expect("frontier");
        assert_eq!(frontier.accepted_next_nonce, 0, "frontier stays frozen");
    }

    #[test]
    fn sim_accepted_foreign_batch_freezes_frontier_with_divergence_marker() {
        // `scheduler_accepts` deliberately omits the canonical scheduler's
        // structural rejections (a frame safe_block not below the inclusion
        // block, non-monotonic frames) under self-trust — the sequencer never
        // produces them. Before the
        // content-identity check, such a foreign batch at the
        // expected nonce would be sim-accepted and silently desync the
        // frontier forever. With the check, it fails the local-batch lookup
        // (kind = foreign), the poison marker persists atomically with the
        // sync, the frontier freezes, and `terminal_fact` reports the
        // divergence.
        let db = temp_db("frontier-future-safe-block");
        let mut storage = set_up_storage(&db.path, Address::repeat_byte(0xCC));

        let future_safe_block_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                user_ops: Vec::new(),
                safe_block: 500,
                fee_price: 0,
            }],
        });

        let batch_submitter = Address::repeat_byte(0xCC);
        let inputs = vec![StoredSafeInput {
            sender: batch_submitter,
            payload: future_safe_block_payload,
            block_number: 100,
        }];
        storage
            .append_safe_inputs(200, inputs.as_slice(), batch_submitter)
            .expect("append commits; the marker rides the same transaction");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(
            frontier.accepted_next_nonce, 0,
            "the frontier must freeze before the diverged landing"
        );
        assert_eq!(
            storage.terminal_fact().expect("terminal fact"),
            Some(crate::storage::TerminalFact::Divergence(Divergence {
                nonce: 0,
                safe_input_index: 0,
            })),
            "the marker is the era's terminal fact"
        );
    }

    #[test]
    fn acceptance_records_post_recovery_batch_at_anchor() {
        // A cockroach-recovered deployment is anchored at N' > 0; its first
        // post-recovery batch lands at nonce N'. The writer must seed `expected`
        // from the anchor and accept it (frontier → N'+1), content-identity
        // passing against the genuine local batch. Every other acceptance test
        // uses anchor=0, so the N' > 0 acceptance path had no coverage.
        let db = temp_db("populate-at-anchor");
        let mut storage = Storage::open_with_era(
            db.path.as_str(),
            &crate::storage::NewEra {
                anchor_batch_nonce: 7,
                ..crate::storage::NewEra::EMPTY_GENESIS
            },
        );
        let submitter = SENDER_A;

        // The era is anchored at N' = 7: build + seal one local batch — with
        // no batch before it, it carries nonce 7 (the anchor).
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        assert_eq!(
            head.batch_nonce(),
            7,
            "the era's first batch carries the anchor nonce N'"
        );
        storage
            .close_frame_and_batch(&mut head)
            .expect("close the nonce-7 batch");
        assert_eq!(
            head.batch_nonce(),
            8,
            "the successor carries the next nonce"
        );

        // Its genuine wire bytes (looked up by nonce N'=7) land from the
        // submitter at a fresh inclusion block.
        let genuine = local_batch_payload(&mut storage, 7);
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: submitter,
                    payload: genuine,
                    block_number: 20,
                }],
                submitter,
            )
            .expect("sync the N' landing");

        assert_eq!(
            storage
                .submitter_frontier()
                .expect("frontier")
                .accepted_next_nonce,
            8,
            "the N'=7 batch is accepted; the frontier advances to 8"
        );
        let divergence: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM canonical_divergence", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(
            divergence, 0,
            "a genuine N' landing must not flag divergence"
        );
    }

    #[test]
    fn acceptance_skips_below_anchor_collapsed_history_without_divergence() {
        // Below the anchor the local list has no batches (that history was folded
        // into S'). Earlier-era L1 landings at nonces < N' are trusted
        // collapsed history: skipped by nonce-mismatch BEFORE the content-identity
        // check, so they must NOT be flagged Foreign (which would freeze the
        // frontier and force a false recovery). Pins the anchor-aware-frontier
        // (I15) skip arm — the inverse of the foreign-batch freeze test above.
        let db = temp_db("populate-below-anchor");
        let mut storage = Storage::open_with_era(
            db.path.as_str(),
            &crate::storage::NewEra {
                anchor_batch_nonce: 7,
                ..crate::storage::NewEra::EMPTY_GENESIS
            },
        );
        let submitter = SENDER_A;

        // Earlier-era batches at nonces 0, 1, 2 (< anchor) land from the submitter.
        let mk = |nonce: u64| {
            ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                nonce,
                frames: vec![sequencer_core::batch::Frame {
                    user_ops: Vec::new(),
                    safe_block: 10,
                    fee_price: 0,
                }],
            })
        };
        let landings: Vec<StoredSafeInput> = (0..3)
            .map(|n| StoredSafeInput {
                sender: submitter,
                payload: mk(n),
                block_number: 20 + n,
            })
            .collect();
        storage
            .append_safe_inputs(30, &landings, submitter)
            .expect("sync below-anchor landings");

        let divergence: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM canonical_divergence", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(
            divergence, 0,
            "below-anchor history must be skipped by nonce-mismatch, not flagged Foreign"
        );
        assert_eq!(
            storage
                .submitter_frontier()
                .expect("frontier")
                .accepted_next_nonce,
            7,
            "nothing is accepted; the frontier stays at the anchor N'"
        );
    }

    #[test]
    fn record_accepted_batches_skips_duplicate_nonces() {
        let db = temp_db("populate-dup-nonces");
        let mut storage = set_up_storage(&db.path, SENDER_A);

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head).expect("close");

        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                20,
                &[
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: landed.clone(),
                        block_number: 20,
                    },
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: landed,
                        block_number: 20,
                    },
                ],
                SENDER_A,
            )
            .expect("append");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(
            frontier.accepted_next_nonce, 1,
            "duplicate nonce must be skipped"
        );
    }

    #[test]
    fn record_accepted_batches_handles_large_nonce_gap() {
        let db = temp_db("populate-nonce-gap");
        let mut storage = set_up_storage(&db.path, SENDER_A);

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head).expect("close");

        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: super::super::test_helpers::make_stale_batch_payload(5, 10),
                    block_number: 20,
                }],
                SENDER_A,
            )
            .expect("append");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.accepted_next_nonce, 0, "gap must stall frontier");
    }

    #[test]
    fn record_accepted_batches_out_of_order_arrivals_stalls_frontier() {
        let db = temp_db("populate-out-of-order");
        let mut storage = set_up_storage(&db.path, SENDER_A);

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head).expect("close");
        storage.close_frame_and_batch(&mut head).expect("close 2");

        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: super::super::test_helpers::make_stale_batch_payload(1, 10),
                    block_number: 20,
                }],
                SENDER_A,
            )
            .expect("append");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(
            frontier.accepted_next_nonce, 0,
            "out of order must stall frontier"
        );

        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                21,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 21,
                }],
                SENDER_A,
            )
            .expect("append nonce 0");

        let frontier2 = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(
            frontier2.accepted_next_nonce, 1,
            "frontier must remain stalled"
        );
    }

    fn accepted_count(storage: &Storage) -> i64 {
        storage
            .conn
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE accepted_input IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("count accepted rows")
    }

    /// A submitter batch at nonce `nonce` with no frames: the simulated
    /// scheduler accepts it at the expected nonce, and no local batch matches.
    fn empty_batch(nonce: u64) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce,
            frames: Vec::new(),
        })
    }

    #[test]
    fn an_append_before_the_era_leaves_the_frontier_empty() {
        use crate::storage::LifecycleCommand;

        let db = temp_db("accepted-before-era");
        let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup)
            .expect("initialize setup");
        // Judged against the empty tree, this landing would read as Foreign.
        assert!(
            scheduler_accepts(
                SENDER_A,
                WINDOW,
                SafeInputView {
                    safe_input_index: 0,
                    sender: SENDER_A,
                    payload: &empty_batch(0),
                    inclusion_block: 10,
                },
                0
            )
            .is_some()
        );
        storage
            .append_safe_inputs(
                20,
                &[
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: empty_batch(0),
                        block_number: 10,
                    },
                    StoredSafeInput {
                        sender: SENDER_B,
                        payload: vec![0xaa],
                        block_number: 15,
                    },
                ],
                SENDER_A,
            )
            .expect("setup's sync appends");

        assert_eq!(storage.safe_input_end_exclusive().expect("inputs"), 2);
        assert_eq!(storage.current_safe_block().expect("head"), Some(20));
        assert_eq!(accepted_count(&storage), 0);
        assert_eq!(storage.canonical_divergence().expect("marker"), None);
    }

    #[test]
    fn an_append_with_an_era_and_no_pinned_identity_fails_and_rolls_back() {
        use crate::storage::IngestedSafeInput;

        let db = temp_db("accepted-era-without-identity");
        // An era with no identity pinned.
        let mut storage = Storage::open_with_era(&db.path, &crate::storage::NewEra::EMPTY_GENESIS);
        let err = storage
            .append_ingested_safe_inputs_with_timestamp(
                20,
                240,
                &[IngestedSafeInput {
                    sender: SENDER_B,
                    payload: vec![0xaa],
                    block_number: 15,
                    block_timestamp: 180,
                    transaction_hash: alloy_primitives::B256::repeat_byte(0x01),
                }],
            )
            .expect_err("the era's frontier needs the pinned submitter");
        assert!(
            crate::storage::is_persistent_storage_error(&err),
            "a missing identity is persistent, got {err:?}"
        );
        assert_eq!(storage.safe_input_end_exclusive().expect("inputs"), 0);
        assert_eq!(storage.current_safe_block().expect("head"), None);
    }

    #[test]
    fn the_first_append_after_the_era_judges_every_input_past_the_stop_block() {
        use crate::storage::{LifecycleCommand, NewEra};

        let db = temp_db("accepted-first-after-era");
        let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup)
            .expect("initialize setup");
        // Setup's sync ingests past the stop block 30: a submitter landing at
        // the anchor nonce 1 inside the baseline (opaque) and one after it.
        storage
            .append_safe_inputs(
                40,
                &[
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: empty_batch(1),
                        block_number: 25,
                    },
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: empty_batch(1),
                        block_number: 35,
                    },
                ],
                SENDER_A,
            )
            .expect("setup's sync appends");
        assert_eq!(accepted_count(&storage), 0);
        assert_eq!(storage.canonical_divergence().expect("marker"), None);

        storage
            .publish_era(
                &db._dir.path().join("baseline"),
                &NewEra {
                    anchor_batch_nonce: 1,
                    ..crate::storage::test_helpers::era(0, 0, 30)
                },
            )
            .expect("publish the era");
        // `run`'s first sync lands nothing new, yet judges the landing past
        // the stop block that setup's sync ingested: no local batch carries
        // nonce 1, so it is Foreign.
        storage
            .append_safe_inputs(41, &[], SENDER_A)
            .expect("run's first sync");
        assert_eq!(
            storage.canonical_divergence().expect("marker"),
            Some(super::Divergence {
                nonce: 1,
                safe_input_index: 1
            }),
            "the post-stop landing is judged; the baseline one is not"
        );
        assert_eq!(accepted_count(&storage), 0);
    }
}
