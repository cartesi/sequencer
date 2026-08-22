-- ---------------------------------------------------------------------------
-- Batch lifecycle
--
-- A batch has two monotonic events in its lifetime, each stored as a nullable
-- write-once timestamp on the row:
--
--   * `sealed_at_ms`      — inclusion lane closed the batch (no more ops).
--   * `invalidated_at_ms` — recovery cascade-invalidated the batch.
--
-- NULL means the event hasn't happened. Once set, triggers below make the
-- column write-once. The only "mutable" state on the row is these two NULL→value
-- transitions, each owned by exactly one writer (inclusion lane vs recovery).
--
-- The **Tip** is the one batch currently accepting ops: sealed_at_ms IS NULL
-- AND invalidated_at_ms IS NULL. A partial unique index enforces at-most-one.
--
-- `nonce` is structural: equal to `parent.nonce + 1`, or 0 for genesis (parent
-- NULL). Enforced by trigger on INSERT. The scheduler's view of a batch's
-- identity; reused across recovery cascades (new Tip forks from last valid
-- ancestor, inheriting nonce via the +1 rule).
-- ---------------------------------------------------------------------------
-- `sealed_at_ms` / `invalidated_at_ms` are observability stamps from the
-- wall clock — write-once (triggers below), but deliberately NOT
-- cross-checked against `created_at_ms`: wall-clock monotonicity is an
-- environmental assumption, not an invariant (NTP steps, VM resume), and a
-- CHECK on it wedged batch close and the recovery cascade in a clock
-- regression (review F8). No production code reads these as values; every
-- reader is an IS NULL / IS NOT NULL predicate.
-- `payload_hash` is the keccak256 of the batch's SSZ wire bytes, stamped at
-- seal time by the same encode path the submitter uses (review R2,
-- hash-at-seal). It is what the content-identity check compares an accepted
-- L1 landing against — and because it is computed by the code that sealed
-- the batch, it survives wire-format upgrades. NULL only while the batch is
-- the open Tip (and on recovery sentinels, which carry no payload).
CREATE TABLE IF NOT EXISTS batches (
    batch_index        INTEGER PRIMARY KEY,
    parent_batch_index INTEGER REFERENCES batches(batch_index),  -- NULL only for genesis
    nonce              INTEGER NOT NULL CHECK (typeof(nonce) = 'integer' AND nonce >= 0),
    created_at_ms      INTEGER NOT NULL,
    sealed_at_ms       INTEGER CHECK (sealed_at_ms IS NULL OR sealed_at_ms >= 0),
    invalidated_at_ms  INTEGER CHECK (invalidated_at_ms IS NULL OR invalidated_at_ms >= 0),
    payload_hash       BLOB CHECK (payload_hash IS NULL OR length(payload_hash) = 32)
);

-- "At most one valid Tip" — structural via partial unique index. The predicate
-- references only local columns of `batches`, so SQLite accepts it.
--
-- We index on COALESCE(sealed_at_ms, 0) instead of sealed_at_ms directly
-- because SQLite UNIQUE indexes treat NULLs as distinct — so indexing directly
-- on `sealed_at_ms` would allow many NULL rows. COALESCE maps all matching
-- rows to the same non-NULL value (0), forcing real uniqueness.
CREATE UNIQUE INDEX IF NOT EXISTS ux_single_valid_tip
    ON batches(COALESCE(sealed_at_ms, 0))
    WHERE sealed_at_ms IS NULL AND invalidated_at_ms IS NULL;

-- Submitter hot path: "give me valid closed batches with nonce >= N", ordered.
CREATE INDEX IF NOT EXISTS idx_batches_valid_closed_by_nonce
    ON batches(nonce)
    WHERE invalidated_at_ms IS NULL AND sealed_at_ms IS NOT NULL;

-- ── Views ──────────────────────────────────────────────────────────────────
CREATE VIEW IF NOT EXISTS valid_batches AS
    SELECT * FROM batches WHERE invalidated_at_ms IS NULL;

CREATE VIEW IF NOT EXISTS valid_closed_batches AS
    SELECT * FROM valid_batches WHERE sealed_at_ms IS NOT NULL;

-- At most one row by the partial unique index above.
CREATE VIEW IF NOT EXISTS valid_open_batch AS
    SELECT * FROM valid_batches WHERE sealed_at_ms IS NULL;

-- Batch-tree anchor: the nonce the single (valid) parentless root carries.
--
-- A genesis deployment is anchored at 0; a cockroach-recovered one is anchored
-- at N' (the post-checkpoint resume nonce), so `run`'s first tip roots at N'
-- without replaying history — there is no separate "sentinel" batch row, the
-- root tip *is* the anchor. The value generalizes the rule the tree already
-- has ("a parentless root carries nonce 0") from a hard-coded 0 to this
-- singleton; it is read by `trg_enforce_nonce_contiguity` (below) and by
-- `compute_next_nonce(parent = None)`.
--
-- Default 0, so a normal deployment is byte-identical to before this table
-- existed. Written exactly once — by `setup` (recovery sets N' before the
-- `setup_complete` marker); `trg_batch_tree_anchor_write_once` freezes it
-- thereafter, since re-anchoring a live deployment would strand its spine.
CREATE TABLE IF NOT EXISTS batch_tree_anchor (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    nonce        INTEGER NOT NULL CHECK (nonce >= 0)
);
INSERT OR IGNORE INTO batch_tree_anchor(singleton_id, nonce) VALUES (0, 0);

-- ── Triggers ───────────────────────────────────────────────────────────────
--
-- These enforce invariants the writer could otherwise violate with a bug.
-- Keep them declarative: each one names an invariant and refuses writes that
-- would break it. The Rust writer is still the source of truth for the
-- transition sequence — triggers just ensure the DB never reaches an
-- inconsistent state if the writer misbehaves.
--
-- typeof() guards: INTEGER affinity does not reject an unconvertible value,
-- and TEXT/BLOB sort above INTEGER, so a bare `x >= 0` CHECK passes 'abc'.
-- Columns that feed SQL-level arithmetic or comparisons (trigger math,
-- frontier folds, lease counts) therefore carry an explicit
-- typeof(x) = 'integer' guard: a mis-bound positional parameter must refuse,
-- not coerce to 0 inside a trigger (H13).

-- Nonce contiguity: `nonce = parent.nonce + 1`, or the batch-tree anchor nonce
-- (0 for a genesis deployment, N' for a recovered one) for the parentless root.
CREATE TRIGGER IF NOT EXISTS trg_enforce_nonce_contiguity
AFTER INSERT ON batches
FOR EACH ROW
BEGIN
    SELECT CASE
        -- A parentless root must carry the deployment's anchor nonce. This is
        -- an *exact* match — tighter than the old "must be 0": a buggy root at
        -- any other nonce still ABORTs, including a re-root at 0 on a recovered
        -- (anchor = N') deployment.
        WHEN NEW.parent_batch_index IS NULL
         AND NEW.nonce != (SELECT nonce FROM batch_tree_anchor WHERE singleton_id = 0)
            THEN RAISE(ABORT, 'parentless root must carry the batch-tree anchor nonce')
        -- At most one *valid* parentless root. A fully-torn cascade invalidates
        -- the old root, then re-roots parentless at the anchor (the documented
        -- `open_fresh_tip_in_tx` parent=None path) — leaving exactly one valid
        -- root again, with the invalidated old root(s) coexisting. (Counts the
        -- just-inserted row, hence `> 1`.)
        WHEN NEW.parent_batch_index IS NULL
         AND (SELECT COUNT(*) FROM batches
              WHERE parent_batch_index IS NULL AND invalidated_at_ms IS NULL) > 1
            THEN RAISE(ABORT, 'at most one valid parentless root per deployment')
        WHEN NEW.parent_batch_index IS NOT NULL
         AND NEW.nonce != (SELECT nonce + 1 FROM batches WHERE batch_index = NEW.parent_batch_index)
            THEN RAISE(ABORT, 'batch nonce must equal parent.nonce + 1')
    END;
END;

-- Write-once: sealed_at_ms transitions only NULL → non-NULL.
CREATE TRIGGER IF NOT EXISTS trg_sealed_at_ms_write_once
BEFORE UPDATE OF sealed_at_ms ON batches
FOR EACH ROW
WHEN OLD.sealed_at_ms IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'sealed_at_ms is write-once');
END;

-- Write-once: invalidated_at_ms transitions only NULL → non-NULL.
CREATE TRIGGER IF NOT EXISTS trg_invalidated_at_ms_write_once
BEFORE UPDATE OF invalidated_at_ms ON batches
FOR EACH ROW
WHEN OLD.invalidated_at_ms IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'invalidated_at_ms is write-once');
END;

-- Write-once: payload_hash transitions only NULL → non-NULL (stamped in the
-- same UPDATE that seals the batch).
CREATE TRIGGER IF NOT EXISTS trg_payload_hash_write_once
BEFORE UPDATE OF payload_hash ON batches
FOR EACH ROW
WHEN OLD.payload_hash IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'payload_hash is write-once');
END;

-- parent_batch_index is immutable after insert.
CREATE TRIGGER IF NOT EXISTS trg_parent_batch_index_immutable
BEFORE UPDATE OF parent_batch_index ON batches
FOR EACH ROW
WHEN (OLD.parent_batch_index IS NULL) != (NEW.parent_batch_index IS NULL)
   OR OLD.parent_batch_index IS NOT NULL AND NEW.parent_batch_index IS NOT NULL
      AND OLD.parent_batch_index != NEW.parent_batch_index
BEGIN
    SELECT RAISE(ABORT, 'parent_batch_index is immutable');
END;

-- nonce is immutable after insert.
CREATE TRIGGER IF NOT EXISTS trg_nonce_immutable
BEFORE UPDATE OF nonce ON batches
FOR EACH ROW
WHEN OLD.nonce != NEW.nonce
BEGIN
    SELECT RAISE(ABORT, 'nonce is immutable');
END;

-- ---------------------------------------------------------------------------
-- Frames and user ops: must target the current Tip.
--
-- These catch "stale WriteHead" bugs — where a writer holds an in-memory
-- batch_index that's no longer the Tip (sealed or invalidated between reads).
-- A PK lookup per row: microseconds, negligible overhead even on hot paths.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS frames (
    batch_index          INTEGER NOT NULL REFERENCES batches(batch_index),
    frame_in_batch       INTEGER NOT NULL CHECK (frame_in_batch >= 0),
    created_at_ms        INTEGER NOT NULL,
    -- Fee committed by the sequencer for this whole frame.
    fee                  INTEGER NOT NULL CHECK (fee >= 0),
    -- Claimed safe L1 block frontier for this frame.
    safe_block           INTEGER NOT NULL CHECK (safe_block >= 0),
    PRIMARY KEY(batch_index, frame_in_batch)
);

CREATE TRIGGER IF NOT EXISTS trg_frames_target_must_be_tip
BEFORE INSERT ON frames
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1 FROM batches
    WHERE batch_index = NEW.batch_index
      AND sealed_at_ms IS NULL
      AND invalidated_at_ms IS NULL
)
BEGIN
    SELECT RAISE(ABORT, 'frames can only be inserted into the current Tip');
END;

CREATE TABLE IF NOT EXISTS user_ops (
    batch_index      INTEGER NOT NULL,
    frame_in_batch   INTEGER NOT NULL,
    pos_in_frame     INTEGER NOT NULL CHECK (pos_in_frame >= 0),
    sender           BLOB NOT NULL CHECK (length(sender) = 20),
    nonce            INTEGER NOT NULL CHECK (nonce >= 0),
    max_fee          INTEGER NOT NULL CHECK (max_fee >= 0),
    data             BLOB NOT NULL,
    sig              BLOB NOT NULL CHECK (length(sig) = 65),
    received_at_ms   INTEGER NOT NULL,
    PRIMARY KEY(batch_index, frame_in_batch, pos_in_frame),
    FOREIGN KEY(batch_index, frame_in_batch) REFERENCES frames(batch_index, frame_in_batch)
);

CREATE TRIGGER IF NOT EXISTS trg_user_ops_target_must_be_tip
BEFORE INSERT ON user_ops
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1 FROM batches
    WHERE batch_index = NEW.batch_index
      AND sealed_at_ms IS NULL
      AND invalidated_at_ms IS NULL
)
BEGIN
    SELECT RAISE(ABORT, 'user_ops can only be inserted into the current Tip');
END;

-- Automatically sequence every user-op into the global replay order on insert.
-- Note: safe_inputs do NOT have an analogous trigger because their
-- batch_index/frame_in_batch are not known at INSERT time — safe inputs
-- are ingested by the input reader independently, and only assigned to a
-- frame when the frame is closed.  The Rust code inserts into
-- sequenced_l2_txs explicitly at frame-close time.
CREATE TRIGGER IF NOT EXISTS trg_sequence_user_op AFTER INSERT ON user_ops
BEGIN
    INSERT INTO sequenced_l2_txs (
        batch_index, frame_in_batch, user_op_pos_in_frame, safe_input_index
    ) VALUES (NEW.batch_index, NEW.frame_in_batch, NEW.pos_in_frame, NULL);
END;

CREATE TABLE IF NOT EXISTS safe_inputs (
    safe_input_index INTEGER PRIMARY KEY,
    sender             BLOB NOT NULL CHECK (length(sender) = 20),
    payload            BLOB NOT NULL,
    -- Block number of the chain block where this direct input was included (e.g. InputAdded event block).
    block_number       INTEGER NOT NULL
        CHECK (typeof(block_number) = 'integer' AND block_number >= 0),
    -- Timestamp of the carrying L1 block.
    block_timestamp    INTEGER NOT NULL CHECK (block_timestamp >= 0),
    -- Hash of the L1 transaction that carried this input.
    transaction_hash   BLOB NOT NULL CHECK (length(transaction_hash) = 32)
);

CREATE INDEX IF NOT EXISTS idx_safe_inputs_sender
    ON safe_inputs(sender);

-- Global append-only replay order consumed by catch-up and feed readers.
-- It is a cache, containing the merged and flattened txs of safe_inputs and user_ops.
CREATE TABLE IF NOT EXISTS sequenced_l2_txs (
    offset               INTEGER PRIMARY KEY,
    batch_index          INTEGER NOT NULL,
    frame_in_batch       INTEGER NOT NULL,

    -- User-op branch: references user_ops(..., pos_in_frame).
    user_op_pos_in_frame INTEGER,

    -- Direct-input branch: references safe_inputs(safe_input_index).
    safe_input_index   INTEGER,

    FOREIGN KEY(batch_index, frame_in_batch)
        REFERENCES frames(batch_index, frame_in_batch),
    FOREIGN KEY(batch_index, frame_in_batch, user_op_pos_in_frame)
        REFERENCES user_ops(batch_index, frame_in_batch, pos_in_frame),
    FOREIGN KEY(safe_input_index)
        REFERENCES safe_inputs(safe_input_index),

    -- XOR invariant: row is either a sequenced user-op OR a drained direct input.
    CHECK (
        (user_op_pos_in_frame IS NOT NULL AND safe_input_index IS NULL) OR
        (user_op_pos_in_frame IS NULL AND safe_input_index IS NOT NULL)
    ),

    -- At most one sequenced user-op row for each user-op key.
    UNIQUE(batch_index, frame_in_batch, user_op_pos_in_frame)
    -- A direct input may be sequenced more than once if its original batch is
    -- invalidated and a recovery batch re-drains it. The read-side query filters
    -- out rows from invalid batches, so only the latest valid drain is visible.
    -- (No UNIQUE constraint on safe_input_index.)
);

CREATE TRIGGER IF NOT EXISTS trg_sequenced_l2_txs_target_must_be_tip
BEFORE INSERT ON sequenced_l2_txs
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1 FROM batches
    WHERE batch_index = NEW.batch_index
      AND sealed_at_ms IS NULL
      AND invalidated_at_ms IS NULL
)
BEGIN
    SELECT RAISE(ABORT, 'sequenced_l2_txs can only target the current Tip');
END;

CREATE INDEX IF NOT EXISTS idx_sequenced_l2_txs_frame
    ON sequenced_l2_txs(batch_index, frame_in_batch);

-- Partial index for efficient MAX(safe_input_index) lookups used to compute
-- the next undrained direct-input cursor at frame-close time.
CREATE INDEX IF NOT EXISTS idx_sequenced_l2_txs_safe_input
    ON sequenced_l2_txs(safe_input_index) WHERE safe_input_index IS NOT NULL;

CREATE VIEW IF NOT EXISTS valid_sequenced_l2_txs AS
SELECT * FROM sequenced_l2_txs
WHERE batch_index NOT IN (SELECT batch_index FROM batches WHERE invalidated_at_ms IS NOT NULL);

-- Derived log of batch submissions the scheduler would actually execute.
-- Unlike a raw log of all safe submissions, this only contains the accepted
-- prefix: batches whose nonce matched the expected sequence and were not stale.
-- Maintained atomically by Storage::append_safe_inputs (via
-- populate_safe_accepted_batches), which simulates the scheduler's
-- acceptance logic over new safe_inputs rows.
CREATE TABLE IF NOT EXISTS safe_accepted_batches (
    safe_input_index     INTEGER PRIMARY KEY REFERENCES safe_inputs(safe_input_index),
    -- CHECK aligns this column with its siblings (batches.nonce, anchor nonce);
    -- the writer is u64-sourced, so a negative value is corruption.
    nonce                INTEGER NOT NULL CHECK (typeof(nonce) = 'integer' AND nonce >= 0),
    first_frame_safe_block INTEGER NOT NULL,
    inclusion_block      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS l1_safe_head (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    -- Highest L1 safe block the input reader has observed and atomically synced into storage.
    block_number INTEGER NOT NULL CHECK (block_number >= 0),
    -- L1 timestamp (Unix seconds) of block_number.
    block_timestamp INTEGER NOT NULL CHECK (block_timestamp >= 0),
    -- Wall-clock time (Unix ms) of the last successful L1 sync.
    -- Used for wall-clock danger estimation when L1 is unreachable.
    synced_at_ms INTEGER NOT NULL CHECK (synced_at_ms >= 0)
);

-- Highest wallet nonce ever broadcast by this deployment's batch-submitter
-- key (review R1a — the durable realization of the TLA+ spec's
-- `walletNonce`). Write-before-broadcast: any component about to send a tx
-- at wallet nonce n first commits watermark = max(watermark, n) — power-loss
-- durable under synchronous=FULL — then sends. Uniform for batch txs and
-- flush no-ops alike, so the flush's slot coverage never depends on the
-- local node's volatile mempool memory (the F1 zombie). Absent row =
-- nothing ever broadcast. Never reset, never lowered.
CREATE TABLE IF NOT EXISTS wallet_nonce_watermark (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    watermark    INTEGER NOT NULL CHECK (watermark >= 0)
);

-- Canonical-divergence poison marker (review R2). Written by the input
-- reader's acceptance simulation — atomically with the sync that detected
-- it — when a fully-accepted L1 landing fails the content-identity check:
-- either no valid closed local batch exists at the accepted nonce
-- (kind = 'foreign': a zombie or foreign batch from our key) or the landed
-- bytes hash differently from ours (kind = 'mismatch'). Once present, the
-- acceptance frontier freezes, `check_danger` reports `CanonicalDivergence`
-- ahead of every other arm, startup refuses, and the runtime detector exits.
-- The remedy is cockroach recovery (wipe + rebuild from L1), never standard
-- recovery — canonical state contains executed effects with no reliable
-- local source. Keep-first: only the earliest detection is recorded.
CREATE TABLE IF NOT EXISTS canonical_divergence (
    singleton_id     INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    nonce            INTEGER NOT NULL CHECK (nonce >= 0),
    safe_input_index INTEGER NOT NULL CHECK (safe_input_index >= 0),
    kind             TEXT    NOT NULL CHECK (kind IN ('foreign', 'mismatch')),
    detected_at_ms   INTEGER NOT NULL CHECK (detected_at_ms >= 0)
);

-- I15 structural enforcement: while the divergence marker exists, the batch
-- tree, promotions, and the pending-snapshot pool are frozen in the engine
-- itself. Standard recovery is forbidden on a diverged frontier; the typed
-- Rust refusals (the local-first startup reducer plus guarded Tip/Cascade
-- mutations and atomic runtime admission) remain the friendly error surface, but these
-- triggers are the enforcement a forgotten call site cannot bypass.
CREATE TRIGGER IF NOT EXISTS trg_batches_frozen_on_divergence_insert
BEFORE INSERT ON batches FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM canonical_divergence WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'batch tree frozen: canonical divergence marker present'); END;

CREATE TRIGGER IF NOT EXISTS trg_batches_frozen_on_divergence_update
BEFORE UPDATE ON batches FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM canonical_divergence WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'batch tree frozen: canonical divergence marker present'); END;

-- External history identity. One database serves exactly one era. The era is
-- minted with the baseline schema; standard recovery advances only the
-- generation. A rebuild's application-history base and durable safe-input
-- drain floor are unknown until the recovered finalized snapshot exists, so
-- they alone start NULL and fill together exactly once before setup completes.
CREATE TABLE IF NOT EXISTS history_state (
    singleton_id                 INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    era_id                       BLOB    NOT NULL CHECK (
        typeof(era_id) = 'blob'
        AND length(era_id) = 16
        AND substr(hex(era_id), 13, 1) = '4'
        AND substr(hex(era_id), 17, 1) IN ('8', '9', 'A', 'B')
    ),
    era_created_at_ms            INTEGER NOT NULL CHECK (
        typeof(era_created_at_ms) = 'integer'
        AND era_created_at_ms >= 0
    ),
    recovery_generation          INTEGER NOT NULL CHECK (
        typeof(recovery_generation) = 'integer'
        AND recovery_generation >= 0
    ),
    base_executed_input_count    INTEGER CHECK (
        base_executed_input_count IS NULL
        OR (
            typeof(base_executed_input_count) = 'integer'
            AND base_executed_input_count >= 0
        )
    ),
    base_safe_input_index        INTEGER CHECK (
        base_safe_input_index IS NULL
        OR (
            typeof(base_safe_input_index) = 'integer'
            AND base_safe_input_index >= 0
        )
    ),
    CHECK (
        (base_executed_input_count IS NULL AND base_safe_input_index IS NULL)
        OR
        (base_executed_input_count IS NOT NULL AND base_safe_input_index IS NOT NULL)
    )
);

CREATE TRIGGER IF NOT EXISTS trg_history_state_single_insert
BEFORE INSERT ON history_state
FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM history_state WHERE singleton_id = 0)
BEGIN
    SELECT RAISE(ABORT, 'history state is inserted once per database');
END;

CREATE TRIGGER IF NOT EXISTS trg_history_identity_write_once
BEFORE UPDATE OF singleton_id, era_id, era_created_at_ms ON history_state
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'history era identity is write-once');
END;

CREATE TRIGGER IF NOT EXISTS trg_history_base_write_once
BEFORE UPDATE OF base_executed_input_count, base_safe_input_index ON history_state
FOR EACH ROW
WHEN OLD.base_executed_input_count IS NOT NULL
  OR OLD.base_safe_input_index IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'history base is write-once');
END;

CREATE TRIGGER IF NOT EXISTS trg_history_generation_monotonic
BEFORE UPDATE OF recovery_generation ON history_state
FOR EACH ROW
WHEN OLD.recovery_generation = 9223372036854775807
  OR NEW.recovery_generation != OLD.recovery_generation + 1
BEGIN
    SELECT RAISE(ABORT, 'recovery generation must advance by exactly one');
END;

CREATE TRIGGER IF NOT EXISTS trg_history_state_not_deletable
BEFORE DELETE ON history_state
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'history state is write-once per database');
END;

-- Canonical application-history coordinates attached to physical replay rows.
--
-- `sequenced_l2_txs.offset` remains the append-only SQLite pagination cursor:
-- it may contain invalidated rows and rows that the application never executes
-- (our own batch submissions and cockroach-root cursor padding). This table is
-- the separate, sparse attribution saying which physical rows did execute and
-- at which `Application::executed_input_count` boundary.
--
-- The primary key makes attribution one-to-one per physical row. This is a
-- derived *current canonical projection*, not the audit log: invalidating a
-- batch atomically deletes its mappings while retaining the physical replay
-- rows. The replacement suffix can then reuse its canonical offsets, enforced
-- by the global logical UNIQUE constraint.
CREATE TABLE IF NOT EXISTS executed_inputs (
    sequenced_l2_tx_offset INTEGER PRIMARY KEY
        REFERENCES sequenced_l2_txs(offset),
    executed_input_offset  INTEGER NOT NULL CHECK (
        typeof(executed_input_offset) = 'integer'
        AND executed_input_offset >= 0
    ),
    UNIQUE(executed_input_offset)
);

-- Invalidation structurally deletes mappings below, so this projection can
-- join the physical table directly without re-running the valid-batch filter.
CREATE VIEW IF NOT EXISTS valid_executed_inputs AS
SELECT
    e.sequenced_l2_tx_offset,
    e.executed_input_offset,
    s.batch_index,
    s.frame_in_batch,
    s.user_op_pos_in_frame,
    s.safe_input_index
FROM executed_inputs e
JOIN sequenced_l2_txs s ON s.offset = e.sequenced_l2_tx_offset;

-- Attribution is creation-time state, not a catch-up repair operation. The
-- Rust writer maps rows in their creation transaction; this backstop limits a
-- target to the current valid Tip and refuses physical-order rewrites.
CREATE TRIGGER IF NOT EXISTS trg_executed_inputs_target_must_be_tip
BEFORE INSERT ON executed_inputs
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1
    FROM sequenced_l2_txs s
    JOIN batches b ON b.batch_index = s.batch_index
    WHERE s.offset = NEW.sequenced_l2_tx_offset
      AND b.sealed_at_ms IS NULL
      AND b.invalidated_at_ms IS NULL
)
BEGIN
    SELECT RAISE(ABORT, 'executed input must target the current valid Tip');
END;

CREATE TRIGGER IF NOT EXISTS trg_executed_inputs_requires_bound_base
BEFORE INSERT ON executed_inputs
FOR EACH ROW
WHEN (SELECT base_executed_input_count FROM history_state WHERE singleton_id = 0) IS NULL
BEGIN
    SELECT RAISE(ABORT, 'executed input history base is not bound');
END;

CREATE TRIGGER IF NOT EXISTS trg_executed_inputs_physical_order
BEFORE INSERT ON executed_inputs
FOR EACH ROW
WHEN EXISTS (
    SELECT 1 FROM executed_inputs
    WHERE sequenced_l2_tx_offset >= NEW.sequenced_l2_tx_offset
)
BEGIN
    SELECT RAISE(ABORT, 'executed input attributions must follow physical replay order');
END;

-- Every new mapping consumes exactly the current canonical next offset:
-- max(the era base, MAX(current mapping) + 1). Invalidation deletes its suffix
-- mappings, naturally rewinding the next offset for the replacement suffix.
CREATE TRIGGER IF NOT EXISTS trg_executed_inputs_contiguous
BEFORE INSERT ON executed_inputs
FOR EACH ROW
WHEN NEW.executed_input_offset != (
    SELECT MAX(
        base_executed_input_count,
        COALESCE((SELECT MAX(executed_input_offset) + 1 FROM executed_inputs), 0)
    )
    FROM history_state
    WHERE singleton_id = 0
)
BEGIN
    SELECT RAISE(ABORT, 'executed input offset must equal canonical next count');
END;

CREATE TRIGGER IF NOT EXISTS trg_executed_inputs_append_only_update
BEFORE UPDATE ON executed_inputs
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'executed input attribution is append-only');
END;

CREATE TRIGGER IF NOT EXISTS trg_protect_valid_executed_input_delete
BEFORE DELETE ON executed_inputs
FOR EACH ROW
WHEN EXISTS (
    SELECT 1
    FROM sequenced_l2_txs s
    JOIN batches b ON b.batch_index = s.batch_index
    WHERE s.offset = OLD.sequenced_l2_tx_offset
      AND b.invalidated_at_ms IS NULL
)
BEGIN
    SELECT RAISE(ABORT, 'valid executed input attribution cannot be deleted');
END;

-- Recovery owns the only deletion path. The batch row is already invalid when
-- this AFTER trigger runs, so the guarded delete above permits exactly these
-- derived mappings to disappear in the same transaction as suffix invalidation.
CREATE TRIGGER IF NOT EXISTS trg_drop_invalidated_executed_inputs
AFTER UPDATE OF invalidated_at_ms ON batches
FOR EACH ROW
WHEN OLD.invalidated_at_ms IS NULL AND NEW.invalidated_at_ms IS NOT NULL
BEGIN
    DELETE FROM executed_inputs
    WHERE sequenced_l2_tx_offset IN (
        SELECT offset FROM sequenced_l2_txs WHERE batch_index = NEW.batch_index
    );
END;

-- Terminal-fault black box: an append-only trail of terminal causes,
-- best-effort recorded before death. DELIBERATELY NOT AN ADMISSION GATE
-- (2026-08-19 review L2; narrowed to this table 2026-08-22, L3): admission
-- is governed by facts — the kernel process lock excludes concurrent
-- owners, `setup_complete` orders commands (two-sided), and
-- `canonical_divergence` is the one absorbing refusal (cockroach rebuild is
-- the only exit). Restart policy after a terminal fault is the R4 exit-code
-- contract (30 = do not restart, page), not a database gate. Nothing reads
-- this table for decisions; it exists so the cause of death travels with
-- the data directory for operator postmortems, surviving log rotation.
CREATE TABLE IF NOT EXISTS terminal_faults (
    fault_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    command        TEXT    NOT NULL CHECK (command IN (
        'setup', 'rebuild', 'run', 'maintenance_flush'
    )),
    cause          TEXT    NOT NULL CHECK (
        typeof(cause) = 'text' AND length(cause) > 0
    ),
    recorded_at_ms INTEGER NOT NULL CHECK (
        typeof(recorded_at_ms) = 'integer' AND recorded_at_ms >= 0
    )
);

CREATE TRIGGER IF NOT EXISTS trg_terminal_faults_append_only_update
BEFORE UPDATE ON terminal_faults
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'terminal-fault black box is append-only');
END;

CREATE TRIGGER IF NOT EXISTS trg_terminal_faults_append_only_delete
BEFORE DELETE ON terminal_faults
FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'terminal-fault black box is append-only');
END;

-- Deployment identity: the persisted DB is only valid for this deployment.
-- Allows L1-unreachable startup after first boot, and prevents interpreting
-- historical sequencer state under a different app or batch-submitter address.
CREATE TABLE IF NOT EXISTS deployment_identity (
    singleton_id              INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    chain_id                  INTEGER NOT NULL CHECK (chain_id > 0),
    app_address               BLOB    NOT NULL CHECK (length(app_address) = 20),
    input_box_address         BLOB    NOT NULL CHECK (length(input_box_address) = 20),
    app_deployment_block      INTEGER NOT NULL CHECK (app_deployment_block >= 0),
    batch_submitter_address   BLOB    NOT NULL CHECK (length(batch_submitter_address) = 20),
    -- Immutable setup-pinned fee source. Fixed has only fixed_log_gas_price;
    -- Uniswap has the complete reviewed token/pool tuple.
    fee_oracle_mode           TEXT    NOT NULL CHECK (fee_oracle_mode IN ('fixed', 'uniswap')),
    fixed_log_gas_price       INTEGER CHECK (fixed_log_gas_price BETWEEN 0 AND 65535),
    fee_oracle_weth           BLOB    CHECK (fee_oracle_weth IS NULL OR length(fee_oracle_weth) = 20),
    fee_oracle_fee_token      BLOB    CHECK (fee_oracle_fee_token IS NULL OR length(fee_oracle_fee_token) = 20),
    fee_oracle_pool           BLOB    CHECK (fee_oracle_pool IS NULL OR length(fee_oracle_pool) = 20),
    fee_oracle_twap_window_secs INTEGER CHECK (fee_oracle_twap_window_secs > 0),
    CHECK (
        (fee_oracle_mode = 'fixed' AND fixed_log_gas_price IS NOT NULL
            AND fee_oracle_weth IS NULL AND fee_oracle_fee_token IS NULL
            AND fee_oracle_pool IS NULL AND fee_oracle_twap_window_secs IS NULL)
        OR
        (fee_oracle_mode = 'uniswap' AND fixed_log_gas_price IS NULL
            AND fee_oracle_weth IS NOT NULL AND fee_oracle_fee_token IS NOT NULL
            AND fee_oracle_pool IS NOT NULL AND fee_oracle_twap_window_secs IS NOT NULL)
    )
);

-- setup-complete marker. The `setup` subcommand
-- pins deployment identity, does the initial L1 sync, and registers the
-- genesis finalized snapshot; it inserts this singleton row as its LAST
-- write. `run` refuses to boot unless the row is present. Presence is the
-- single linearization point for "setup finished": it distinguishes a clean
-- setup from one that crashed midway (identity pinned and/or genesis
-- snapshot registered, but the marker absent), which every prior setup step
-- is individually idempotent enough to let `setup` re-run and complete.
CREATE TABLE IF NOT EXISTS setup_complete (
    singleton_id    INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    completed_at_ms INTEGER NOT NULL CHECK (completed_at_ms >= 0)
);

-- The batch-tree anchor is frozen once setup completes. `setup` writes it (0
-- by default, N' on recovery) before inserting the `setup_complete` marker;
-- after the marker exists, re-anchoring a live deployment would strand the
-- existing batch spine, so any further UPDATE aborts. Defense-in-depth: the
-- `setup --recovery` path is already a strict one-shot on a fresh DB.
CREATE TRIGGER IF NOT EXISTS trg_batch_tree_anchor_write_once
BEFORE UPDATE OF nonce ON batch_tree_anchor
FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM setup_complete WHERE singleton_id = 0)
BEGIN
    SELECT RAISE(ABORT, 'batch-tree anchor is frozen after setup completes');
END;


-- ---------------------------------------------------------------------------
-- Batch policy singleton
--
-- Every value is a log-space exponent with base 129/128 (see sequencer_core::fee).
-- Exponent N represents a linear value of (129/128)^N.
--
-- Fee unit:
--   `gas_price` is denominated in "fee-token smallest units per L1 gas unit"
--   (application-defined ERC-20 X; the wallet prototype starts with USDC).
--   The L1 fee oracle converts base+priority gas (wei) via a pinned Uniswap V3
--   WETH/X TWAP and encodes the exact quote to log space. The tenfold safety
--   margin lives in `log_slack` (not in the oracle). Local Anvil uses an
--   explicit fixed exponent instead of Uniswap.
--
-- Fee derivation (view):
--   log_recommended_fee = log_gas_price + log_slack + log_one_plus_alpha
--                       + log_delta + log_user_op_bytes
--   Pure addition — no overflow possible. The oracle feeds log_gas_price
--   directly in log space. Alpha amortizes batch fixed gas over expected
--   target occupancy (not retrospective per-op settlement).
--
-- Batch sizing (view):
--   log_batch_size_target = log_base_gas - log_alpha - log_delta
--   batch_size_target = base_gas / (alpha * delta). The inclusion lane converts
--   to linear bytes via fee_to_linear() for its byte-count comparison.
--
-- Batch size invariant (CHECK):
--   log_base_gas - log_alpha - log_delta < log_max_batch_bytes
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS batch_policy (
    singleton_id             INTEGER PRIMARY KEY CHECK (singleton_id = 0),

    -- Knobs (operator-tunable, set via set_alpha(num, denom)):
    -- log_alpha = log(alpha) where alpha = num/denom. Can be negative (alpha < 1).
    log_alpha                INTEGER NOT NULL,
    -- log_one_plus_alpha = log(1 + alpha). Always >= 0.
    log_one_plus_alpha       INTEGER NOT NULL CHECK (log_one_plus_alpha >= 0),
    -- Log-space fee exponent fed by the oracle.
    log_gas_price            INTEGER NOT NULL CHECK (log_gas_price >= 0),
    -- Unix-ms of the last successful oracle (or Fixed setup) write.
    -- 0 means never written; Uniswap treats that as stale.
    log_gas_price_updated_at_ms INTEGER NOT NULL CHECK (log_gas_price_updated_at_ms >= 0),
    -- log_{129/128}(10), rounded by log_fee_ratio(10, 1) = 296.
    -- This tenfold price slack is applied in log space, not in the oracle.
    log_slack                INTEGER NOT NULL CHECK (log_slack >= 0),

    -- Constants (log-space):
    log_base_gas             INTEGER NOT NULL CHECK (log_base_gas > 0),
    log_delta                INTEGER NOT NULL CHECK (log_delta > 0),
    log_user_op_bytes        INTEGER NOT NULL CHECK (log_user_op_bytes > 0),
    log_max_batch_bytes      INTEGER NOT NULL CHECK (log_max_batch_bytes > 0),

    CHECK (log_base_gas - log_alpha - log_delta < log_max_batch_bytes)
);

-- Default values. All log-space exponents with base 129/128:
--   log_alpha            = log_{129/128}(0.168)          = -229
--   log_one_plus_alpha   = log_{129/128}(1.168)          = 20
--   log_base_gas         = log_{129/128}(55000)          = 1403
--   log_delta            = log_{129/128}(26)             = 419
--   log_user_op_bytes    = log_{129/128}(126)            = 621
--   log_max_batch_bytes  = log_{129/128}(32000)          = 1333
--
-- Derived by view:
--   log_recommended_fee  = 0 + 296 + 20 + 419 + 621      = 1356
--   log_batch_size_target = 1403 - (-229) - 419           = 1213
INSERT OR IGNORE INTO batch_policy(
    singleton_id,

    log_alpha, log_one_plus_alpha, log_gas_price, log_gas_price_updated_at_ms, log_slack,

    log_base_gas, log_delta, log_user_op_bytes, log_max_batch_bytes
)
VALUES (
    0,

    -229, 20, 0, 0, 296,

    1403, 419, 621, 1333
);

-- Derived view for reads. All outputs are log-space exponents (base 129/128).
CREATE VIEW IF NOT EXISTS batch_policy_derived AS
SELECT *,
    -- Fee per user-op byte.
    log_gas_price + log_slack + log_one_plus_alpha + log_delta + log_user_op_bytes
        AS log_recommended_fee,
    -- Batch size target in log-space (convert via fee_to_linear for bytes).
    log_base_gas - log_alpha - log_delta
        AS log_batch_size_target
FROM batch_policy;

-- ---------------------------------------------------------------------------
-- Snapshot dumps
--
-- Three tables together implement the snapshot lifecycle:
--
--   * dumps                — master table: every on-disk dump has a row here,
--                            with a lease_count tracking in-flight readers
--                            (typically HTTP handlers streaming the dump).
--                            Rows are FK-referenced by pending_snapshots and
--                            finalized_snapshot via ON DELETE RESTRICT, so a
--                            dump can't be removed while still referenced.
--   * pending_snapshots    — one row per batch that has been closed and
--                            dumped, but not yet observed landed on L1. Keyed
--                            by nonce so the inclusion lane can match its own
--                            batches in the direct-input stream.
--   * finalized_snapshot   — single-row table holding the latest L1-finalized
--                            snapshot. INSERT OR REPLACE on promotion;
--                            consumers (the watchdog) read this row to learn
--                            which dump corresponds to the canonical state.
--
-- Garbage collection: dumps with lease_count = 0 AND no row in either
-- pending_snapshots or finalized_snapshot are eligible for filesystem +
-- DB-row removal. The Rust caller drives this; the FK constraints prevent
-- accidental deletion while a reference still exists.
--
-- Lifecycle:
--   * batch close:        INSERT into dumps; INSERT into pending_snapshots.
--   * batch observed:     INSERT OR REPLACE into finalized_snapshot; DELETE
--                         the promoted nonces from pending_snapshots in one tx.
--                         The previous finalized's dump becomes GC-eligible.
--   * cascade invalidate: DELETE from pending_snapshots; sweep dumps via GC.
--   * HTTP serving:       acquire/release lease_count to prevent GC during
--                         in-flight streams.
--   * startup:            UPDATE dumps SET lease_count = 0 (clear stale
--                         in-process leases from a crashed previous run).
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS dumps (
    id           INTEGER PRIMARY KEY,
    prefix       TEXT NOT NULL UNIQUE,
    lease_count  INTEGER NOT NULL DEFAULT 0
        CHECK (typeof(lease_count) = 'integer' AND lease_count >= 0)
);

CREATE TABLE IF NOT EXISTS pending_snapshots (
    nonce                 INTEGER PRIMARY KEY CHECK (typeof(nonce) = 'integer' AND nonce >= 0),
    dump_id               INTEGER NOT NULL REFERENCES dumps(id) ON DELETE RESTRICT,
    l2_tx_index           INTEGER NOT NULL
        CHECK (typeof(l2_tx_index) = 'integer' AND l2_tx_index >= 0),
    executed_input_count  INTEGER NOT NULL CHECK (
        typeof(executed_input_count) = 'integer'
        AND executed_input_count >= 0
    )
);

CREATE TABLE IF NOT EXISTS finalized_snapshot (
    singleton_id          INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    dump_id               INTEGER NOT NULL REFERENCES dumps(id) ON DELETE RESTRICT,
    inclusion_block       INTEGER NOT NULL
        CHECK (typeof(inclusion_block) = 'integer' AND inclusion_block >= 0),
    l2_tx_index           INTEGER NOT NULL
        CHECK (typeof(l2_tx_index) = 'integer' AND l2_tx_index >= 0),
    executed_input_count  INTEGER NOT NULL CHECK (
        typeof(executed_input_count) = 'integer'
        AND executed_input_count >= 0
    )
);

-- I15 structural enforcement, snapshot half (batch-tree half lives next to
-- the canonical_divergence table): promotions and pending-pool clears are
-- frozen while the divergence marker exists.
CREATE TRIGGER IF NOT EXISTS trg_promotion_frozen_on_divergence
BEFORE INSERT ON finalized_snapshot FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM canonical_divergence WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'promotion frozen: canonical divergence marker present'); END;

CREATE TRIGGER IF NOT EXISTS trg_pending_clear_frozen_on_divergence
BEFORE DELETE ON pending_snapshots FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM canonical_divergence WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'pending-snapshot clear frozen: canonical divergence marker present'); END;
