// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Schema-invariant regression tests: the triggers, CHECKs and partial unique
//! indexes in `migrations/0001_schema.sql` enforce the batch invariants, and
//! each test pins one tripwire.

use rusqlite::params;

use super::super::test_helpers::{
    RecoveryFixture, SENDER_A, SENDER_B, default_protocol_timing, local_batch_payload,
    open_temp_storage, set_up_storage,
};
use crate::storage::{SafeInputRange, Storage, StoredSafeInput};

const NONCE_RULE: &str = "a batch nonce is the last valid nonce + 1, or the era anchor";
const LIFECYCLE_RULE: &str = "a batch changes only by sealing once with its hash, by being \
                              accepted once while sealed, or by invalidating once while unaccepted";
const ACCEPTANCE_RULE: &str = "an acceptance extends the accepted prefix of the valid path";
const FROZEN_RULE: &str = "batch tree frozen: canonical divergence marker present";

fn assert_refused(storage: &Storage, sql: &str, message: &str) {
    let err = storage.conn.execute(sql, []);
    assert!(
        format!("{err:?}").contains(message),
        "`{sql}` must abort with {message:?}, got: {err:?}"
    );
}

#[test]
fn schema_rejects_second_valid_tip() {
    // The partial unique index `ux_single_valid_tip` catches a writer that
    // opens a new Tip without sealing the old one first.
    let (_db, mut storage) = open_temp_storage("schema-second-tip");
    storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");

    // Try to bypass the lane and insert a second valid Tip directly, at the
    // right nonce so only the Tip index can refuse it.
    let err = storage.conn.execute(
        "INSERT INTO batches (batch_index, nonce, created_at_ms) VALUES (99, 1, 1000)",
        [],
    );
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UNIQUE constraint failed") && msg.contains("ux_single_valid_tip"),
        "expected ux_single_valid_tip violation, got: {msg}"
    );
}

#[test]
fn schema_rejects_a_wrong_batch_nonce() {
    // A new batch carries the last valid nonce + 1. Insert the bad-nonce
    // batch as already sealed so it doesn't collide with the Tip on
    // `ux_single_valid_tip`.
    let (_db, mut storage) = open_temp_storage("schema-bad-nonce");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage
        .close_batch_at(&mut head, 0)
        .expect("close batch 0; batch 1 is now Tip");
    // Batch 1 has nonce 1, so the next is 2.
    assert_refused(
        &storage,
        "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms, payload_hash) \
         VALUES (999, 99, 100, 100, zeroblob(32))",
        NONCE_RULE,
    );
}

#[test]
fn schema_rejects_a_batch_before_the_era_exists() {
    // With no valid batch and no era both sides of the COALESCE are NULL,
    // `IS NOT` holds, and every insert ABORTs, whatever its nonce.
    let db = crate::test_support::temp_db("schema-batch-without-era");
    let storage =
        Storage::initialize_for_command(db.path.as_str(), crate::storage::LifecycleCommand::Setup)
            .expect("setup schema, no era");
    for nonce in [0, 7] {
        assert_refused(
            &storage,
            &format!(
                "INSERT INTO batches (batch_index, nonce, created_at_ms) VALUES (0, {nonce}, 100)"
            ),
            NONCE_RULE,
        );
    }
}

#[test]
fn schema_rejects_genesis_with_nonzero_nonce() {
    // The genesis era's anchor is 0, so a first batch at a non-zero nonce
    // ABORTs.
    let (_db, storage) = open_temp_storage("schema-genesis-nonzero");
    assert_refused(
        &storage,
        "INSERT INTO batches (batch_index, nonce, created_at_ms) VALUES (0, 7, 100)",
        NONCE_RULE,
    );
}

#[test]
fn schema_first_batch_carries_the_era_anchor() {
    // With an era anchored at N' (a recovered deployment), the first batch
    // carries exactly N', and the next N' + 1. (Rows are inserted sealed so
    // the single-Tip index, which only covers open rows, doesn't mask the
    // trigger under test.)
    let db = crate::test_support::temp_db("schema-anchor-first");
    let storage = Storage::open_with_era(
        db.path.as_str(),
        &crate::storage::NewEra {
            anchor_batch_nonce: 42,
            ..crate::storage::NewEra::EMPTY_GENESIS
        },
    );
    assert_refused(
        &storage,
        "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms, payload_hash) \
         VALUES (0, 0, 100, 100, zeroblob(32))",
        NONCE_RULE,
    );
    storage
        .conn
        .execute(
            "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms, payload_hash) \
             VALUES (0, 42, 100, 100, zeroblob(32))",
            [],
        )
        .expect("the first batch at the anchor nonce is allowed");
    assert_refused(
        &storage,
        "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms, payload_hash) \
         VALUES (1, 42, 100, 100, zeroblob(32))",
        NONCE_RULE,
    );
    storage
        .conn
        .execute(
            "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms, payload_hash) \
             VALUES (1, 43, 100, 100, zeroblob(32))",
            [],
        )
        .expect("the next batch carries the anchor + 1");
}

#[test]
fn schema_reuses_the_anchor_after_a_full_tear() {
    // A cascade that invalidates every valid batch leaves no last valid
    // nonce, so the next batch carries the era's anchor again.
    let db = crate::test_support::temp_db("schema-anchor-after-tear");
    let storage = Storage::open_with_era(
        db.path.as_str(),
        &crate::storage::NewEra {
            anchor_batch_nonce: 5,
            ..crate::storage::NewEra::EMPTY_GENESIS
        },
    );
    storage
        .conn
        .execute_batch(
            "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms, payload_hash) \
             VALUES (0, 5, 100, 100, zeroblob(32));
             INSERT INTO batches (batch_index, nonce, created_at_ms) VALUES (1, 6, 100);
             UPDATE batches SET invalidated_at_ms = 200 WHERE invalidated_at_ms IS NULL;",
        )
        .expect("two batches, then a full tear");
    assert_refused(
        &storage,
        "INSERT INTO batches (nonce, created_at_ms) VALUES (7, 300)",
        NONCE_RULE,
    );
    storage
        .conn
        .execute(
            "INSERT INTO batches (nonce, created_at_ms) VALUES (5, 300)",
            [],
        )
        .expect("the batch after a full tear carries the anchor");
}

#[test]
fn schema_rejects_a_second_valid_batch_at_an_existing_nonce() {
    // The nonce trigger refuses a duplicate valid nonce first (it is never
    // the last valid nonce + 1); `ux_valid_batch_nonce` refuses it on its own.
    let (_db, storage) = open_temp_storage("schema-duplicate-nonce");
    storage
        .conn
        .execute(
            "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms, payload_hash) \
             VALUES (0, 0, 100, 100, zeroblob(32))",
            [],
        )
        .expect("first batch");
    let duplicate = "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms, \
                     payload_hash) VALUES (1, 0, 100, 100, zeroblob(32))";
    assert_refused(&storage, duplicate, NONCE_RULE);
    storage
        .conn
        .execute_batch("DROP TRIGGER trg_batch_insert")
        .expect("isolate the index");
    let msg = format!("{:?}", storage.conn.execute(duplicate, []));
    assert!(
        msg.contains("UNIQUE constraint failed") && msg.contains("batches.nonce"),
        "expected ux_valid_batch_nonce violation, got: {msg}"
    );
}

#[test]
fn schema_rejects_a_sealed_batch_without_its_hash() {
    // The CHECK pairs `sealed_at_ms` with `payload_hash` on every row. On an
    // UPDATE the lifecycle trigger (a BEFORE trigger) answers before it.
    let (_db, mut storage) = open_temp_storage("schema-seal-without-hash");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage.close_batch_at(&mut head, 0).expect("close batch 0");
    assert_refused(
        &storage,
        "INSERT INTO batches (batch_index, nonce, created_at_ms, sealed_at_ms) \
         VALUES (99, 2, 100, 100)",
        "CHECK constraint failed",
    );
    assert_refused(
        &storage,
        "INSERT INTO batches (batch_index, nonce, created_at_ms, payload_hash) \
         VALUES (99, 2, 100, zeroblob(32))",
        "CHECK constraint failed",
    );
    assert_refused(
        &storage,
        "UPDATE batches SET sealed_at_ms = 100 WHERE batch_index = 1",
        LIFECYCLE_RULE,
    );
}

// ── The batch lifecycle (`trg_batch_lifecycle`) ──────────────────────────
//
// The fixture holds batch 0 sealed and batch 1 the open Tip.

fn lifecycle_fixture(name: &str) -> (crate::test_support::TestDb, Storage) {
    let (db, mut storage) = open_temp_storage(name);
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage.close_batch_at(&mut head, 0).expect("close batch 0");
    (db, storage)
}

#[test]
fn lifecycle_allows_sealing_the_tip_with_its_hash() {
    let (_db, storage) = lifecycle_fixture("lifecycle-seal");
    storage
        .conn
        .execute(
            "UPDATE batches SET sealed_at_ms = 100, payload_hash = zeroblob(32) \
             WHERE batch_index = 1",
            [],
        )
        .expect("the seal arm admits the open Tip's seal");
}

#[test]
fn lifecycle_allows_invalidating_sealed_and_open_batches_once() {
    // The cascade's shape: one UPDATE over a sealed batch and the open Tip.
    let (_db, storage) = lifecycle_fixture("lifecycle-invalidate");
    let changed = storage
        .conn
        .execute(
            "UPDATE batches SET invalidated_at_ms = 100 \
             WHERE batch_index >= 0 AND invalidated_at_ms IS NULL",
            [],
        )
        .expect("the invalidation arm admits the cascade");
    assert_eq!(changed, 2);
}

#[test]
fn lifecycle_rejects_re_seal() {
    let (_db, storage) = lifecycle_fixture("lifecycle-re-seal");
    assert_refused(
        &storage,
        "UPDATE batches SET sealed_at_ms = sealed_at_ms + 1 WHERE batch_index = 0",
        LIFECYCLE_RULE,
    );
}

#[test]
fn lifecycle_rejects_a_payload_hash_rewrite() {
    // payload_hash is the content-identity anchor for the canonical-
    // divergence check; a rewrite would let a foreign/zombie L1 landing
    // false-match a local batch.
    let (_db, storage) = lifecycle_fixture("lifecycle-hash-rewrite");
    assert_refused(
        &storage,
        "UPDATE batches SET payload_hash = randomblob(32) WHERE batch_index = 0",
        LIFECYCLE_RULE,
    );
}

#[test]
fn lifecycle_rejects_re_invalidate() {
    let (_db, mut storage) = lifecycle_fixture("lifecycle-re-invalidate");
    storage.insert_invalid_batch(1).expect("first invalidate");
    assert_refused(
        &storage,
        "UPDATE batches SET invalidated_at_ms = invalidated_at_ms + 1 WHERE batch_index = 1",
        LIFECYCLE_RULE,
    );
}

#[test]
fn lifecycle_rejects_un_invalidate() {
    let (_db, mut storage) = lifecycle_fixture("lifecycle-un-invalidate");
    storage.insert_invalid_batch(1).expect("invalidate");
    assert_refused(
        &storage,
        "UPDATE batches SET invalidated_at_ms = NULL WHERE batch_index = 1",
        LIFECYCLE_RULE,
    );
}

#[test]
fn lifecycle_rejects_a_nonce_change() {
    // Nonce immutability underpins the frontier and contiguity invariants.
    let (_db, storage) = lifecycle_fixture("lifecycle-nonce");
    assert_refused(
        &storage,
        "UPDATE batches SET nonce = nonce + 1 WHERE batch_index = 0",
        LIFECYCLE_RULE,
    );
}

#[test]
fn lifecycle_rejects_a_created_at_change() {
    let (_db, storage) = lifecycle_fixture("lifecycle-created-at");
    assert_refused(
        &storage,
        "UPDATE batches SET created_at_ms = created_at_ms + 1 WHERE batch_index = 0",
        LIFECYCLE_RULE,
    );
}

#[test]
fn lifecycle_rejects_a_batch_index_change() {
    let (_db, storage) = lifecycle_fixture("lifecycle-batch-index");
    assert_refused(
        &storage,
        "UPDATE batches SET batch_index = 99 WHERE batch_index = 0",
        LIFECYCLE_RULE,
    );
}

#[test]
fn lifecycle_rejects_sealing_an_invalidated_batch() {
    let (_db, mut storage) = lifecycle_fixture("lifecycle-seal-invalidated");
    storage.insert_invalid_batch(1).expect("invalidate the Tip");
    assert_refused(
        &storage,
        "UPDATE batches SET sealed_at_ms = 100, payload_hash = zeroblob(32) \
         WHERE batch_index = 1",
        LIFECYCLE_RULE,
    );
}

#[test]
fn lifecycle_rejects_seal_and_invalidate_in_one_statement() {
    let (_db, storage) = lifecycle_fixture("lifecycle-seal-and-invalidate");
    assert_refused(
        &storage,
        "UPDATE batches SET sealed_at_ms = 100, payload_hash = zeroblob(32), \
         invalidated_at_ms = 100 WHERE batch_index = 1",
        LIFECYCLE_RULE,
    );
}

// ── Acceptance (`trg_batch_acceptance_extends_the_prefix`, `trg_batch_lifecycle`) ──
//
// The fixture's era is anchored at 7: batches 0, 1, 2 are sealed (nonces 7,
// 8, 9), batch 3 is the open Tip (nonce 10), and safe inputs 0..=5 exist.

fn acceptance_fixture(name: &str) -> (crate::test_support::TestDb, Storage) {
    let db = crate::test_support::temp_db(name);
    let mut storage = Storage::open_with_era(
        db.path.as_str(),
        &crate::storage::NewEra {
            anchor_batch_nonce: 7,
            ..crate::storage::NewEra::EMPTY_GENESIS
        },
    );
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    for _ in 0..3 {
        storage.close_batch_at(&mut head, 0).expect("close a batch");
    }
    for index in 0..=5 {
        storage
            .conn
            .execute(
                "INSERT INTO safe_inputs \
                 (safe_input_index, sender, payload, block_number, block_timestamp, transaction_hash) \
                 VALUES (?1, zeroblob(20), X'', 10, 0, zeroblob(32))",
                [index],
            )
            .expect("insert a safe input");
    }
    (db, storage)
}

fn accept(storage: &Storage, batch_index: i64, input: i64) -> rusqlite::Result<usize> {
    storage.conn.execute(
        "UPDATE batches SET accepted_input = ?1 WHERE batch_index = ?2",
        [input, batch_index],
    )
}

#[test]
fn acceptance_extends_the_prefix_from_the_anchor() {
    let (_db, storage) = acceptance_fixture("acceptance-prefix");
    assert_eq!(accept(&storage, 0, 1).expect("the batch at the anchor"), 1);
    assert_eq!(accept(&storage, 1, 3).expect("the next batch, later"), 1);
}

#[test]
fn a_batch_is_never_inserted_accepted() {
    let (_db, storage) = acceptance_fixture("acceptance-insert");
    // Nonce 11 follows the Tip's 10, so only the acceptance arm can refuse.
    assert_refused(
        &storage,
        "INSERT INTO batches (nonce, created_at_ms, sealed_at_ms, payload_hash, accepted_input) \
         VALUES (11, 0, 1, zeroblob(32), 1)",
        "a batch is inserted unaccepted",
    );
    // Without the trigger, the table still refuses an acceptance on a batch
    // that is not sealed.
    storage
        .conn
        .execute_batch("DROP TRIGGER trg_batch_insert")
        .expect("drop the insert trigger");
    assert_refused(
        &storage,
        "INSERT INTO batches (nonce, created_at_ms, accepted_input) VALUES (11, 0, 1)",
        "CHECK constraint failed",
    );
}

#[test]
fn acceptance_rejects_skipping_a_nonce() {
    let (_db, storage) = acceptance_fixture("acceptance-skip");
    assert_refused(
        &storage,
        "UPDATE batches SET accepted_input = 1 WHERE batch_index = 1",
        ACCEPTANCE_RULE,
    );
    accept(&storage, 0, 1).expect("the batch at the anchor");
    assert_refused(
        &storage,
        "UPDATE batches SET accepted_input = 2 WHERE batch_index = 2",
        ACCEPTANCE_RULE,
    );
}

#[test]
fn acceptance_rejects_an_input_not_after_the_last_acceptance() {
    let (_db, storage) = acceptance_fixture("acceptance-earlier-input");
    accept(&storage, 0, 3).expect("the batch at the anchor");
    for input in [2, 3] {
        assert_refused(
            &storage,
            &format!("UPDATE batches SET accepted_input = {input} WHERE batch_index = 1"),
            ACCEPTANCE_RULE,
        );
    }
}

#[test]
fn acceptance_rejects_an_unsealed_batch() {
    let (_db, storage) = acceptance_fixture("acceptance-unsealed");
    accept(&storage, 0, 1).expect("accept nonce 7");
    accept(&storage, 1, 2).expect("accept nonce 8");
    accept(&storage, 2, 3).expect("accept nonce 9");
    // The Tip carries the frontier nonce 10, so only the lifecycle refuses.
    assert_refused(
        &storage,
        "UPDATE batches SET accepted_input = 4 WHERE batch_index = 3",
        LIFECYCLE_RULE,
    );
}

#[test]
fn acceptance_rejects_an_invalidated_batch() {
    let (_db, storage) = acceptance_fixture("acceptance-invalidated");
    storage
        .conn
        .execute("UPDATE batches SET invalidated_at_ms = 100", [])
        .expect("a full tear of unaccepted batches");
    // Batch 0 carries the anchor nonce, so only the lifecycle refuses.
    assert_refused(
        &storage,
        "UPDATE batches SET accepted_input = 1 WHERE batch_index = 0",
        LIFECYCLE_RULE,
    );
}

#[test]
fn acceptance_is_written_once() {
    let (_db, storage) = acceptance_fixture("acceptance-twice");
    accept(&storage, 0, 1).expect("the batch at the anchor");
    for sql in [
        "UPDATE batches SET accepted_input = 2 WHERE batch_index = 0",
        "UPDATE batches SET accepted_input = NULL WHERE batch_index = 0",
    ] {
        assert_refused(&storage, sql, LIFECYCLE_RULE);
    }
}

#[test]
fn acceptance_changes_nothing_else() {
    let (_db, storage) = acceptance_fixture("acceptance-alone");
    assert_refused(
        &storage,
        "UPDATE batches SET accepted_input = 1, invalidated_at_ms = 100 WHERE batch_index = 0",
        LIFECYCLE_RULE,
    );
}

#[test]
fn an_accepted_batch_is_never_invalidated() {
    let (_db, storage) = acceptance_fixture("acceptance-invalidate");
    accept(&storage, 0, 1).expect("the batch at the anchor");
    assert_refused(
        &storage,
        "UPDATE batches SET invalidated_at_ms = 100 WHERE batch_index = 0",
        LIFECYCLE_RULE,
    );
    // The cascade from the first unaccepted batch is admitted.
    let changed = storage
        .conn
        .execute(
            "UPDATE batches SET invalidated_at_ms = 100 \
             WHERE batch_index >= 1 AND invalidated_at_ms IS NULL",
            [],
        )
        .expect("the cascade past the accepted prefix");
    assert_eq!(changed, 3);
}

#[test]
fn the_divergence_freeze_answers_an_acceptance_first() {
    let (_db, mut storage) = acceptance_fixture("acceptance-frozen");
    crate::storage::test_helpers::record_canonical_divergence(&mut storage, 7, 1);
    // A well-formed acceptance, then one that breaks both other rules.
    assert_refused(
        &storage,
        "UPDATE batches SET accepted_input = 1 WHERE batch_index = 0",
        FROZEN_RULE,
    );
    assert_refused(
        &storage,
        "UPDATE batches SET accepted_input = 1 WHERE batch_index = 3",
        FROZEN_RULE,
    );
}

#[test]
fn schema_rejects_frame_insert_into_sealed_batch() {
    // This is the bug class we've been fighting: writer holds a stale
    // WriteHead and writes to a batch that's no longer the Tip.
    let (_db, mut storage) = open_temp_storage("schema-frame-into-sealed");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage
        .close_batch_at(&mut head, 0)
        .expect("close batch 0; batch 0 is now sealed");
    // Batch 0 is sealed. Any direct insert into its frames must fail.
    let err = storage.conn.execute(
        "INSERT INTO frames (batch_index, frame_in_batch, created_at_ms, fee, safe_block) \
         VALUES (0, 1, 100, 1356, 0)",
        [],
    );
    assert!(
        format!("{err:?}").contains("frames can only be inserted into the current Tip"),
        "expected tip-only-frames trigger, got: {err:?}"
    );
}

#[test]
fn schema_rejects_frame_insert_into_invalidated_batch() {
    let (_db, mut storage) = open_temp_storage("schema-frame-into-invalid");
    storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    // Invalidate (without sealing) — Tip that never closed, now dead.
    storage.insert_invalid_batch(0).expect("invalidate tip");
    let err = storage.conn.execute(
        "INSERT INTO frames (batch_index, frame_in_batch, created_at_ms, fee, safe_block) \
         VALUES (0, 1, 100, 1356, 0)",
        [],
    );
    assert!(
        format!("{err:?}").contains("frames can only be inserted into the current Tip"),
        "expected tip-only-frames trigger, got: {err:?}"
    );
}

#[test]
fn schema_rejects_application_input_into_non_tip() {
    // The third sibling of the tip-only triggers (frames + user_ops already
    // have negative tests; application_inputs did not). The global replay order
    // is the source for recovery re-drain and catch-up; a stale-WriteHead row
    // into a sealed batch would corrupt it.
    let (_db, mut storage) = open_temp_storage("schema-sequenced-into-sealed");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage
        .close_batch_at(&mut head, 0)
        .expect("close batch 0; it is now sealed (no longer the Tip)");
    // Target the sealed batch's existing frame (0, 0). The BEFORE-INSERT
    // trigger fires before the safe_input_index FK is evaluated.
    let err = storage.conn.execute(
        "INSERT INTO application_inputs \
         (l2_tx_count, batch_index, frame_in_batch, user_op_pos_in_frame, safe_input_index) \
         VALUES (0, 0, 0, NULL, 0)",
        [],
    );
    assert!(
        format!("{err:?}").contains("application input must target the current valid Tip"),
        "expected tip-only-sequenced trigger, got: {err:?}"
    );
}

#[test]
fn nonce_reuse_after_partial_cascade() {
    // After a cascade that invalidates only the suffix (keeping a prefix
    // valid), the new Tip follows the last valid batch, so its nonce is that
    // batch's `nonce + 1` — the same nonce the invalidated suffix's first
    // batch had. Nonce reuse is automatic.
    //
    // Scenario: batch 0 is accepted (the accepted frontier advances past
    // nonce 0). Batch 1 is stale and triggers cascade. Batches 1, 2, 3
    // invalidated; batch 0 remains valid.
    let (_db, mut storage) = open_temp_storage("nonce-reuse-partial-cascade");

    let mut head = storage
        .initialize_open_state(10, SafeInputRange::empty_at(0))
        .expect("initialize at safe_block=10");
    storage
        .close_batch_at(&mut head, 100)
        .expect("close batch 0 (nonce 0)");
    storage
        .close_batch_at(&mut head, 100)
        .expect("close batch 1 (nonce 1)");
    storage
        .close_batch_at(&mut head, 100)
        .expect("close batch 2 (nonce 2)");
    // Head is now batch 3 (nonce 3, first_frame_safe_block=100).

    // Batch 0 lands on L1 (accepted): safe_input at block 20 with nonce 0.
    let landed = local_batch_payload(&mut storage, 0);
    storage.land(20, &[(SENDER_A, landed)]); // append batch 0 submission
    // Advance safe head so batches 1, 2, 3 (first_frame=100) are stale.
    // current_safe=1400 → 1400-100=1300 >= 1200.
    storage.advance_safe_head(1400); // advance past threshold

    let inv = storage.recover_post_flush(1200).expect("recover");
    // Batches 1, 2, 3 invalidated; batch 0 (accepted) stays valid.
    assert_eq!(
        inv,
        crate::storage::test_helpers::batch_ids(&[1, 2, 3]),
        "only the suffix cascades, got {inv:?}"
    );

    // The new Tip (batch 4) reuses the pivot's nonce 1: the last valid
    // nonce (0) + 1.
    let (tip_index, tip_nonce): (i64, i64) = storage
        .conn
        .query_row(
            "SELECT batch_index, nonce FROM valid_open_batch",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query recovery tip");
    assert_eq!(tip_index, 4, "the recovery Tip takes a fresh batch_index");
    assert_eq!(tip_nonce, 1, "recovery Tip reuses nonce 1");
}

// ── CHECK-constraint regressions ──────────────────────────
//
// These differ from the trigger-based tests above: they exercise raw
// `CHECK` clauses declared in `migrations/0001_schema.sql`. The
// type-safe `Storage` API would reject these values Rust-side; we go
// through `storage.conn.execute` to prove the schema itself refuses.

#[test]
fn schema_rejects_safe_input_with_wrong_sender_length() {
    // `safe_inputs.sender` must be exactly 20 bytes (an
    // Ethereum address). A shorter or longer blob must be refused
    // by the schema even if it bypasses the Rust API.
    let (_db, storage) = open_temp_storage("schema-safe-input-sender-len");
    let err = storage.conn.execute(
        "INSERT INTO safe_inputs \
             (safe_input_index, sender, payload, block_number, block_timestamp, transaction_hash) \
             VALUES (0, X'DEADBEEF', X'00', 10, 0, zeroblob(32))",
        [],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed"),
        "expected CHECK constraint error on safe_inputs.sender, got: {err:?}",
    );
}

#[test]
fn schema_rejects_user_op_with_wrong_sender_length() {
    // `user_ops.sender` must be 20 bytes.
    let (_db, storage) = open_temp_storage("schema-user-op-sender-len");
    // Seed a frame to satisfy the composite FK — initialize_open_state
    // creates batch 0 frame 0 as the Tip.
    let mut storage = storage;
    storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    let err = storage.conn.execute(
            "INSERT INTO user_ops \
             (batch_index, frame_in_batch, pos_in_frame, sender, nonce, max_fee, data, sig, received_at_ms) \
             VALUES (0, 0, 0, X'010203', 0, 0, X'', ?1, 0)",
            params![vec![0u8; 65]],
        );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed"),
        "expected CHECK constraint error on user_ops.sender length, got: {err:?}",
    );
}

#[test]
fn schema_rejects_user_op_with_wrong_signature_length() {
    // `user_ops.sig` must be exactly 65 bytes (secp256k1
    // r || s || v). Regression for "accidentally accepted a non-65
    // signature and crashed a downstream consumer."
    let (_db, mut storage) = open_temp_storage("schema-user-op-sig-len");
    storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    let valid_sender = vec![0u8; 20];
    let short_sig = vec![0u8; 32]; // Should be 65.
    let err = storage.conn.execute(
            "INSERT INTO user_ops \
             (batch_index, frame_in_batch, pos_in_frame, sender, nonce, max_fee, data, sig, received_at_ms) \
             VALUES (0, 0, 0, ?1, 0, 0, X'', ?2, 0)",
            params![valid_sender, short_sig],
        );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed"),
        "expected CHECK constraint error on user_ops.sig length, got: {err:?}",
    );
}

#[test]
fn schema_rejects_application_input_with_neither_xor_branch() {
    // `application_inputs` must be either a user-op row
    // (user_op_pos_in_frame IS NOT NULL) or a direct-input row
    // (safe_input_index IS NOT NULL), never both and never neither.
    // Setting both to NULL is the clean XOR violation to test —
    // FKs are only triggered on non-NULL values so we isolate the
    // CHECK constraint.
    let (_db, mut storage) = open_temp_storage("schema-sequenced-l2-tx-xor-neither");
    storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    let err = storage.conn.execute(
        "INSERT INTO application_inputs \
             (l2_tx_count, batch_index, frame_in_batch, user_op_pos_in_frame, safe_input_index) \
             VALUES (0, 0, 0, NULL, NULL)",
        [],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed"),
        "expected CHECK constraint error on application_inputs XOR, got: {err:?}",
    );
}

#[test]
fn schema_rejects_deployment_identity_with_zero_chain_id() {
    // `deployment_identity.chain_id > 0`. chain_id = 0 would
    // collide with the EIP-712 domain's unspecified-chain sentinel
    // and break signature recovery; the CHECK refuses to persist it
    // in the first place.
    let db = crate::test_support::temp_db("schema-deployment-chain-id-zero");
    // No identity yet: the test inserts the row itself.
    let storage = Storage::open_with_era(&db.path, &crate::storage::NewEra::EMPTY_GENESIS);
    let address = vec![0u8; 20];
    let err = storage.conn.execute(
        "INSERT INTO deployment_identity \
             (singleton_id, chain_id, app_address, input_box_address, \
              app_deployment_block, batch_submitter_address, max_direct_payload_bytes, \
              max_wait_blocks, fee_oracle_mode, fixed_log_gas_price, \
              fee_oracle_weth, fee_oracle_fee_token, fee_oracle_pool, \
              fee_oracle_twap_window_secs) \
             VALUES (0, 0, ?1, ?1, 0, ?1, 1024, 1200, 'fixed', 0, NULL, NULL, NULL, NULL)",
        params![address],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed: chain_id > 0"),
        "expected CHECK constraint error on chain_id > 0, got: {err:?}",
    );
}

#[test]
fn schema_rejects_deployment_identity_with_negative_direct_payload_cap() {
    let db = crate::test_support::temp_db("schema-deployment-negative-cap");
    // No identity yet: the test inserts the row itself.
    let storage = Storage::open_with_era(&db.path, &crate::storage::NewEra::EMPTY_GENESIS);
    let address = vec![0u8; 20];
    let err = storage.conn.execute(
        "INSERT INTO deployment_identity \
             (singleton_id, chain_id, app_address, input_box_address, \
              app_deployment_block, batch_submitter_address, max_direct_payload_bytes, \
              max_wait_blocks, fee_oracle_mode, fixed_log_gas_price, \
              fee_oracle_weth, fee_oracle_fee_token, fee_oracle_pool, \
              fee_oracle_twap_window_secs) \
             VALUES (0, 1, ?1, ?1, 0, ?1, -1, 1200, 'fixed', 0, NULL, NULL, NULL, NULL)",
        params![address],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed: max_direct_payload_bytes >= 0"),
        "expected CHECK constraint error on max_direct_payload_bytes >= 0, got: {err:?}",
    );
}

#[test]
fn schema_rejects_deployment_identity_with_a_zero_window() {
    // A window of zero would make every batch stale on arrival, and the
    // scheduler refuses to start with one.
    let db = crate::test_support::temp_db("schema-deployment-zero-window");
    // No identity yet: the test inserts the row itself.
    let storage = Storage::open_with_era(&db.path, &crate::storage::NewEra::EMPTY_GENESIS);
    let address = vec![0u8; 20];
    let err = storage.conn.execute(
        "INSERT INTO deployment_identity \
             (singleton_id, chain_id, app_address, input_box_address, \
              app_deployment_block, batch_submitter_address, max_direct_payload_bytes, \
              max_wait_blocks, fee_oracle_mode, fixed_log_gas_price, \
              fee_oracle_weth, fee_oracle_fee_token, fee_oracle_pool, \
              fee_oracle_twap_window_secs) \
             VALUES (0, 1, ?1, ?1, 0, ?1, 1024, 0, 'fixed', 0, NULL, NULL, NULL, NULL)",
        params![address],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed: max_wait_blocks >= 1"),
        "expected CHECK constraint error on max_wait_blocks >= 1, got: {err:?}",
    );
}

#[test]
fn schema_rejects_safe_input_with_negative_block_number() {
    // `safe_inputs.block_number >= 0`. Catches a regression
    // that would let a negative block number slip through — the rest
    // of the system assumes non-negative and could panic on cast.
    let (_db, storage) = open_temp_storage("schema-safe-input-neg-block");
    let sender = vec![0u8; 20];
    let err = storage.conn.execute(
        "INSERT INTO safe_inputs \
             (safe_input_index, sender, payload, block_number, block_timestamp, transaction_hash) \
             VALUES (0, ?1, X'00', -1, 0, zeroblob(32))",
        params![sender],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed"),
        "expected CHECK constraint error on block_number >= 0, got: {err:?}",
    );
}

#[test]
fn divergence_marker_freezes_batch_tree_promotions_and_pending_clears() {
    // I15 structural enforcement: with the canonical-divergence marker
    // present, the trg_*_frozen_on_divergence family must RAISE on batch
    // inserts/updates, promotion inserts, and pending-snapshot deletes —
    // even for a caller that bypassed every typed Rust refusal.
    let (_db, mut storage) = open_temp_storage("schema-divergence-freeze");
    let mut head = storage
        .initialize_open_state(10, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    storage
        .close_batch_at(&mut head, 10)
        .expect("close batch 0 before freezing");
    storage
        .insert_batch_snapshot(std::path::Path::new("/tmp/pending-fixture"), 0)
        .expect("insert a pending dump row to attempt deleting");
    crate::storage::test_helpers::record_canonical_divergence(&mut storage, 0, 0);

    let frozen = |err: rusqlite::Error| {
        let text = format!("{err}");
        assert!(
            text.contains("frozen") && text.contains("divergence"),
            "expected a divergence-freeze RAISE, got: {text}"
        );
    };

    // Nonce 0 breaks the nonce rule too (the next is 2): the freeze answers
    // first, as the most recently created BEFORE INSERT trigger.
    frozen(
        storage
            .conn
            .execute(
                "INSERT INTO batches (batch_index, nonce, created_at_ms) VALUES (99, 0, 0)",
                [],
            )
            .expect_err("batch INSERT must be frozen"),
    );
    // Likewise before the lifecycle trigger: this nonce change breaks both.
    frozen(
        storage
            .conn
            .execute("UPDATE batches SET nonce = 7 WHERE batch_index = 0", [])
            .expect_err("batch UPDATE must be frozen"),
    );
    frozen(
        storage
            .conn
            .execute(
                "UPDATE batches SET invalidated_at_ms = 1 WHERE invalidated_at_ms IS NULL",
                [],
            )
            .expect_err("batch UPDATE (cascade shape) must be frozen"),
    );
    frozen(
        storage
            .conn
            .execute("DELETE FROM snapshots", [])
            .expect_err("pending-snapshot DELETE must be frozen"),
    );

    // Reads stay usable on a frozen DB: the danger check must still
    // classify the divergence (the boot-refusal path depends on it).
    let protocol = default_protocol_timing();
    let status = storage
        .check_danger(&protocol, 1_000_000)
        .expect("check danger on a frozen DB");
    assert_eq!(status, crate::storage::DangerStatus::CanonicalDivergence(0));
}

#[test]
fn schema_rejects_safe_input_with_negative_block_timestamp() {
    let (_db, storage) = open_temp_storage("schema-safe-input-neg-block-timestamp");
    let sender = vec![0u8; 20];
    let err = storage.conn.execute(
        "INSERT INTO safe_inputs \
             (safe_input_index, sender, payload, block_number, block_timestamp, transaction_hash) \
             VALUES (0, ?1, X'00', 10, -1, zeroblob(32))",
        params![sender],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed"),
        "expected CHECK constraint error on block_timestamp >= 0, got: {err:?}",
    );
}

#[test]
fn schema_rejects_safe_input_with_wrong_transaction_hash_length() {
    let (_db, storage) = open_temp_storage("schema-safe-input-transaction-hash-len");
    let sender = vec![0u8; 20];
    let err = storage.conn.execute(
        "INSERT INTO safe_inputs \
             (safe_input_index, sender, payload, block_number, block_timestamp, transaction_hash) \
             VALUES (0, ?1, X'00', 10, 0, X'DEADBEEF')",
        params![sender],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed"),
        "expected CHECK constraint error on transaction_hash length, got: {err:?}",
    );
}

/// CHECK constraint guards against alpha values that would push the batch-size
/// target past `log_max_batch_bytes`.
#[test]
fn batch_policy_check_rejects_unsafe_alpha() {
    let db = crate::test_support::temp_db("unsafe-alpha");
    let storage = set_up_storage(&db.path, SENDER_A);
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

/// The seeded `log_slack` is the exponent nearest a tenfold price slack, as
/// its schema comment says.
#[test]
fn seeded_log_slack_is_the_exponent_nearest_tenfold() {
    use alloy_primitives::U256;
    use sequencer_core::fee::fee_to_linear;

    let db = crate::test_support::temp_db("seeded-log-slack");
    let storage = set_up_storage(&db.path, SENDER_A);
    let slack: u16 = storage
        .conn
        .query_row(
            "SELECT log_slack FROM batch_policy WHERE singleton_id = 0",
            [],
            |row| row.get(0),
        )
        .expect("read seeded log_slack");
    // Nearest in log space: r^(2·slack−1) <= 10² < r^(2·slack+1), r = 129/128.
    // At this base the linear fees are about 3.3e13 units, so flooring is far
    // below the margins.
    let base = 4000;
    let raised = fee_to_linear(base + slack);
    let raised_squared = raised * raised;
    let hundredfold_base_squared = fee_to_linear(base) * fee_to_linear(base) * U256::from(100u64);
    assert!(raised_squared * U256::from(128u64) <= hundredfold_base_squared * U256::from(129u64));
    assert!(hundredfold_base_squared * U256::from(128u64) < raised_squared * U256::from(129u64));
}

#[test]
fn schema_rejects_safe_input_update_and_delete() {
    let db = crate::test_support::temp_db("safe-inputs-append-only");
    let mut storage = set_up_storage(&db.path, SENDER_A);
    let inputs = vec![
        StoredSafeInput {
            sender: SENDER_B,
            payload: vec![0x01],
            block_number: 10,
        },
        StoredSafeInput {
            sender: SENDER_B,
            payload: vec![0x02],
            block_number: 10,
        },
    ];
    storage
        .append_safe_inputs(10, inputs.as_slice(), SENDER_A, &default_protocol_timing())
        .expect("seed safe inputs");

    let update = storage
        .conn
        .execute(
            "UPDATE safe_inputs SET payload = X'03' WHERE safe_input_index = 1",
            [],
        )
        .expect_err("a stored input must not change");
    assert!(
        update.to_string().contains("safe inputs are immutable"),
        "{update}"
    );
    // Index 0 is a hole below the head, which no production reader detects.
    let delete = storage
        .conn
        .execute("DELETE FROM safe_inputs WHERE safe_input_index = 0", [])
        .expect_err("a stored input must not disappear");
    assert!(delete.to_string().contains("retained"), "{delete}");

    assert_eq!(storage.safe_input_end_exclusive().expect("end"), 2);
    assert_eq!(
        storage.safe_input_count_through_block(10).expect("count"),
        2
    );
}

#[test]
fn every_schema_table_is_strict() {
    // STRICT is what refuses a mis-classed value before any trigger runs, so a
    // table added without it would reopen that hole.
    let (_db, storage) = open_temp_storage("schema-every-table-strict");
    let mut stmt = storage
        .conn
        .prepare(
            "SELECT name FROM pragma_table_list \
             WHERE type = 'table' AND strict = 0 AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("prepare table list");
    let lax: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .expect("list tables")
        .collect::<rusqlite::Result<_>>()
        .expect("read table names");
    assert!(lax.is_empty(), "tables without STRICT: {lax:?}");
}

#[test]
fn strict_type_refusal_is_a_persistent_storage_error() {
    let (_db, storage) = open_temp_storage("schema-strict-type-refusal");
    let err = storage
        .conn
        .execute(
            "INSERT INTO wallet_nonce_watermark (singleton_id, watermark) VALUES (0, ?1)",
            ["not-an-integer"],
        )
        .expect_err("STRICT must refuse text in an INTEGER column");
    let rusqlite::Error::SqliteFailure(source, _) = &err else {
        panic!("expected a SQLite failure, got {err:?}");
    };
    assert_eq!(
        source.extended_code,
        rusqlite::ffi::SQLITE_CONSTRAINT_DATATYPE,
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("cannot store TEXT value in INTEGER column"),
        "{err}"
    );
    assert!(crate::storage::convert::is_persistent_storage_error(&err));
}
