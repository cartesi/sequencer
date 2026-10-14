// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Schema-invariant regression tests: the triggers, CHECKs and partial unique
//! indexes in `migrations/0001_schema.sql` enforce the batch-tree invariants, and
//! each test pins one tripwire.

use rusqlite::params;

use super::super::test_helpers::{
    RecoveryFixture, SENDER_A, SENDER_B, default_protocol_timing, local_batch_payload,
    open_temp_storage, temp_db_with_default_deployment_identity as temp_db,
};
use crate::storage::{SafeInputRange, Storage, StoredSafeInput};

#[test]
fn schema_rejects_second_valid_tip() {
    // The partial unique index `ux_single_valid_tip` catches a writer that
    // opens a new Tip without sealing the old one first.
    let (_db, mut storage) = open_temp_storage("schema-second-tip");
    storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");

    // Try to bypass the lane and insert a second valid Tip directly.
    let err = storage.conn.execute(
        "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms) \
         VALUES (99, 0, 1, 1000)",
        [],
    );
    let msg = format!("{err:?}");
    assert!(
        msg.contains("UNIQUE constraint failed") && msg.contains("ux_single_valid_tip"),
        "expected ux_single_valid_tip violation, got: {msg}"
    );
}

#[test]
fn schema_rejects_bad_nonce_contiguity() {
    // Nonce must equal parent.nonce + 1 — trigger enforces it.
    // Insert the bad-nonce batch as already-sealed so it doesn't collide
    // with the existing Tip on `ux_single_valid_tip`.
    let (_db, mut storage) = open_temp_storage("schema-bad-nonce");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage
        .close_batch_at(&mut head, 0)
        .expect("close batch 0; batch 1 is now Tip");
    // Batch 1 has nonce 1 (0 + 1). Insert child with nonce 99 (should be 2).
    let err = storage.conn.execute(
        "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms, sealed_at_ms) \
         VALUES (999, 1, 99, \
                 (SELECT created_at_ms FROM batches WHERE batch_index = 1), \
                 (SELECT created_at_ms FROM batches WHERE batch_index = 1))",
        [],
    );
    assert!(
        format!("{err:?}").contains("batch nonce must equal parent.nonce + 1"),
        "expected nonce trigger, got: {err:?}"
    );
}

#[test]
fn schema_rejects_genesis_with_nonzero_nonce() {
    // Default anchor is 0, so a parentless root at a non-zero nonce ABORTs
    // (genesis must be 0) — the exact-match form of the contiguity rule.
    let (_db, storage) = open_temp_storage("schema-genesis-nonzero");
    let err = storage.conn.execute(
        "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms) \
         VALUES (0, NULL, 7, 100)",
        [],
    );
    assert!(
        format!("{err:?}").contains("parentless root must carry the batch-tree anchor nonce"),
        "expected anchor-nonce trigger, got: {err:?}"
    );
}

#[test]
fn schema_allows_parentless_root_at_anchor_nonce() {
    // With the anchor set to N' (a recovered deployment), a parentless root
    // at N' is permitted — the recovery generalization of "genesis at 0".
    // (Rows are inserted sealed so the single-Tip partial index, which only
    // covers open rows, doesn't mask the contiguity trigger under test.)
    let db = temp_db("schema-anchor-root");
    let storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .conn
        .execute(
            "UPDATE batch_tree_anchor SET nonce = 42 WHERE singleton_id = 0",
            [],
        )
        .expect("set anchor");
    // A root at the anchor nonce (42) is accepted...
    storage
        .conn
        .execute(
            "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms, sealed_at_ms) \
             VALUES (0, NULL, 42, 100, 100)",
            [],
        )
        .expect("parentless root at the anchor nonce is allowed");
    // ...but a root at 0 ABORTs: the rule is an exact match against the anchor.
    let err = storage.conn.execute(
        "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms, sealed_at_ms) \
         VALUES (1, NULL, 0, 100, 100)",
        [],
    );
    assert!(
        format!("{err:?}").contains("parentless root must carry the batch-tree anchor nonce"),
        "a root at the wrong nonce must abort, got: {err:?}"
    );
}

#[test]
fn schema_rejects_second_valid_parentless_root() {
    // At most one *valid* parentless root: a second parentless insert (both
    // valid, both at the anchor nonce 0) ABORTs on the single-root guard.
    // (Sealed inserts isolate this from the single-Tip index; a fully-torn
    // cascade re-roots only after invalidating the old root, covered below.)
    let db = temp_db("schema-two-roots");
    let storage = Storage::open(db.path.as_str()).expect("open storage");
    storage
        .conn
        .execute(
            "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms, sealed_at_ms) \
             VALUES (0, NULL, 0, 100, 100)",
            [],
        )
        .expect("first root ok");
    let err = storage.conn.execute(
        "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms, sealed_at_ms) \
         VALUES (1, NULL, 0, 100, 100)",
        [],
    );
    assert!(
        format!("{err:?}").contains("at most one valid parentless root"),
        "expected single-root guard, got: {err:?}"
    );
    // Invalidate the first root, then a second parentless root is allowed —
    // exactly the fully-torn-cascade re-root shape.
    storage
        .conn
        .execute(
            "UPDATE batches SET invalidated_at_ms = 200 WHERE batch_index = 0",
            [],
        )
        .expect("invalidate first root");
    storage
        .conn
        .execute(
            "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms, sealed_at_ms) \
             VALUES (1, NULL, 0, 100, 100)",
            [],
        )
        .expect("re-root parentless after the old root is invalidated");
}

#[test]
fn schema_freezes_anchor_after_setup_complete() {
    // The anchor is write-once: once `setup_complete` exists, UPDATEs abort.
    let db = temp_db("schema-anchor-frozen");
    let storage = Storage::open(db.path.as_str()).expect("open storage");
    // Pre-marker: UPDATE is allowed (this is what recovery setup does).
    storage
        .conn
        .execute(
            "UPDATE batch_tree_anchor SET nonce = 7 WHERE singleton_id = 0",
            [],
        )
        .expect("anchor settable before setup_complete");
    storage
        .conn
        .execute(
            "INSERT INTO setup_complete (singleton_id, completed_at_ms) VALUES (0, 1)",
            [],
        )
        .expect("mark setup complete");
    let err = storage.conn.execute(
        "UPDATE batch_tree_anchor SET nonce = 8 WHERE singleton_id = 0",
        [],
    );
    assert!(
        format!("{err:?}").contains("batch-tree anchor is frozen after setup completes"),
        "expected anchor-freeze trigger, got: {err:?}"
    );
}

#[test]
fn schema_rejects_re_seal() {
    let (_db, mut storage) = open_temp_storage("schema-re-seal");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage
        .close_batch_at(&mut head, 0)
        .expect("close batch 0 (seals it)");
    // Batch 0 is sealed. Attempt to re-seal with a different timestamp.
    let err = storage.conn.execute(
        "UPDATE batches SET sealed_at_ms = sealed_at_ms + 1 WHERE batch_index = 0",
        [],
    );
    assert!(
        format!("{err:?}").contains("sealed_at_ms is write-once"),
        "expected write-once trigger, got: {err:?}"
    );
}

#[test]
fn schema_rejects_re_invalidate() {
    let (_db, mut storage) = open_temp_storage("schema-re-invalidate");
    storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    // Seed via test helper (uses now_unix_ms internally).
    storage.insert_invalid_batch(0).expect("first invalidate");
    let err = storage.conn.execute(
        "UPDATE batches SET invalidated_at_ms = invalidated_at_ms + 1 \
         WHERE batch_index = 0",
        [],
    );
    assert!(
        format!("{err:?}").contains("invalidated_at_ms is write-once"),
        "expected write-once trigger, got: {err:?}"
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
fn schema_rejects_parent_batch_index_mutation() {
    let (_db, mut storage) = open_temp_storage("schema-parent-immutable");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage.close_batch_at(&mut head, 0).expect("close batch 0");
    // Try to change parent of batch 1 — should be rejected.
    let err = storage.conn.execute(
        "UPDATE batches SET parent_batch_index = NULL WHERE batch_index = 1",
        [],
    );
    assert!(
        format!("{err:?}").contains("parent_batch_index is immutable"),
        "expected parent-immutable trigger, got: {err:?}"
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
         (offset, batch_index, frame_in_batch, user_op_pos_in_frame, safe_input_index) \
         VALUES (0, 0, 0, NULL, 0)",
        [],
    );
    assert!(
        format!("{err:?}").contains("application input must target the current valid Tip"),
        "expected tip-only-sequenced trigger, got: {err:?}"
    );
}

#[test]
fn schema_rejects_nonce_mutation() {
    // Nonce immutability underpins the frontier + contiguity invariants.
    let (_db, mut storage) = open_temp_storage("schema-nonce-immutable");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage.close_batch_at(&mut head, 0).expect("close batch 0");
    let err = storage.conn.execute(
        "UPDATE batches SET nonce = nonce + 1 WHERE batch_index = 0",
        [],
    );
    assert!(
        format!("{err:?}").contains("nonce is immutable"),
        "expected nonce-immutable trigger, got: {err:?}"
    );
}

#[test]
fn schema_rejects_payload_hash_rewrite() {
    // payload_hash is the content-identity anchor for the canonical-
    // divergence check; a rewrite would let a foreign/zombie L1 landing
    // false-match a local batch. Write-once once stamped at seal.
    let (_db, mut storage) = open_temp_storage("schema-payload-hash-write-once");
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize");
    storage
        .close_batch_at(&mut head, 0)
        .expect("close batch 0; payload_hash is stamped at seal");
    let err = storage.conn.execute(
        "UPDATE batches SET payload_hash = \
         X'0000000000000000000000000000000000000000000000000000000000000000' \
         WHERE batch_index = 0",
        [],
    );
    assert!(
        format!("{err:?}").contains("payload_hash is write-once"),
        "expected payload-hash-write-once trigger, got: {err:?}"
    );
}

#[test]
fn nonce_reuse_after_cascade_with_valid_ancestor() {
    // Beautiful part of parent-pointer + structural nonce: after a cascade
    // that invalidates only the suffix (keeping an ancestor valid), the
    // new Tip's parent is the last valid ancestor, so its nonce is
    // `ancestor.nonce + 1` — the same nonce the invalidated suffix's
    // first batch had. Nonce reuse is automatic.
    //
    // Scenario: batch 0 is accepted (safe_accepted_batches advances past
    // nonce 0). Batch 1 is stale and triggers cascade. Batches 1, 2, 3
    // invalidated; batch 0 remains valid.
    let (_db, mut storage) = open_temp_storage("nonce-reuse-with-ancestor");

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
    assert_eq!(inv, vec![1, 2, 3], "only the suffix cascades, got {inv:?}");

    // The NEW Tip has parent=0 (the last valid ancestor), nonce=1.
    // This is what nonce reuse looks like: the invalidated batch 1 had
    // nonce 1; the recovery batch gets the same nonce via +1-from-parent.
    let (tip_nonce, tip_parent): (i64, i64) = storage
        .conn
        .query_row(
            "SELECT nonce, parent_batch_index FROM valid_open_batch",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query recovery tip");
    assert_eq!(tip_nonce, 1, "recovery Tip reuses nonce 1");
    assert_eq!(tip_parent, 0, "recovery Tip's parent is batch 0");
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
             (offset, batch_index, frame_in_batch, user_op_pos_in_frame, safe_input_index) \
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
    let storage = Storage::open(db.path.as_str()).expect("open storage");
    let address = vec![0u8; 20];
    let err = storage.conn.execute(
        "INSERT INTO deployment_identity \
             (singleton_id, chain_id, app_address, input_box_address, \
              app_deployment_block, batch_submitter_address, \
              fee_oracle_mode, fixed_log_gas_price, \
              fee_oracle_weth, fee_oracle_fee_token, fee_oracle_pool, \
              fee_oracle_twap_window_secs) \
             VALUES (0, 0, ?1, ?1, 0, ?1, 'fixed', 0, NULL, NULL, NULL, NULL)",
        params![address],
    );
    assert!(
        format!("{err:?}").contains("CHECK constraint failed"),
        "expected CHECK constraint error on chain_id > 0, got: {err:?}",
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

    frozen(
        storage
            .conn
            .execute(
                "INSERT INTO batches (batch_index, parent_batch_index, nonce, created_at_ms) \
                 VALUES (99, NULL, 0, 0)",
                [],
            )
            .expect_err("batch INSERT must be frozen"),
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
    let storage = Storage::open(db.path.as_str()).expect("open storage");
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
    let storage = Storage::open(db.path.as_str()).expect("open storage");
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
    let mut storage = Storage::open(db.path.as_str()).expect("open storage");
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
        storage
            .safe_inputs_in_block_range(9, 10)
            .expect("read back"),
        inputs
    );
}
