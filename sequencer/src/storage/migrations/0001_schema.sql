-- ---------------------------------------------------------------------------
-- Batch lifecycle
--
-- A batch has three monotonic events in its lifetime, each stored as a
-- nullable write-once column on the row:
--
--   * `sealed_at_ms`      — inclusion lane closed the batch (no more ops).
--   * `accepted_input`    — input reader saw the scheduler accept the batch's
--                           landing at that safe input (I9).
--   * `invalidated_at_ms` — recovery cascade-invalidated the batch.
--
-- NULL means the event hasn't happened. `trg_batch_lifecycle` below makes each
-- write-once. The only "mutable" state on the row is these three NULL→value
-- transitions, each owned by exactly one writer (inclusion lane, input reader,
-- recovery). An accepted batch is never invalidated.
--
-- The **Tip** is the one batch currently accepting ops: sealed_at_ms IS NULL
-- AND invalidated_at_ms IS NULL. A partial unique index enforces at-most-one.
--
-- The valid batches form a list in `batch_index` order. `nonce` is the
-- scheduler's view of a batch's identity: the last valid nonce + 1, or the
-- era's `anchor_batch_nonce` (0 at genesis) when no batch is valid. Enforced by
-- trigger on INSERT, and unique among valid batches. A recovery cascade
-- invalidates a suffix; the replacement batch follows the last valid batch and
-- reuses the first invalidated nonce.
-- ---------------------------------------------------------------------------
-- `sealed_at_ms` / `invalidated_at_ms` are write-once observability stamps from
-- the wall clock, deliberately not cross-checked against `created_at_ms`:
-- wall-clock order is environmental, not an invariant (NTP steps, VM resume), and
-- such a CHECK would wedge batch close and the recovery cascade on a clock
-- regression. Every reader is an IS NULL / IS NOT NULL predicate.
-- `payload_hash` is the keccak256 of the batch's SSZ wire bytes, stamped at
-- seal time by the encode path the submitter uses. The content-identity check
-- compares an accepted L1 landing against it; because the sealing code computed
-- it, it survives wire-format upgrades. NULL exactly on a never-sealed batch
-- (the open Tip, or a Tip a cascade invalidated).
-- `accepted_input` is the safe input whose landing of this batch the scheduler
-- accepted; the accepted batches are a prefix of the valid path, in input
-- order, and the greatest one is the accepted frontier. Its UNIQUE index
-- serves the frontier reads.
CREATE TABLE IF NOT EXISTS batches (
    batch_index        INTEGER PRIMARY KEY,
    nonce              INTEGER NOT NULL CHECK (nonce >= 0),
    created_at_ms      INTEGER NOT NULL,
    sealed_at_ms       INTEGER CHECK (sealed_at_ms IS NULL OR sealed_at_ms >= 0),
    invalidated_at_ms  INTEGER CHECK (invalidated_at_ms IS NULL OR invalidated_at_ms >= 0),
    payload_hash       BLOB CHECK (payload_hash IS NULL OR length(payload_hash) = 32),
    accepted_input     INTEGER UNIQUE REFERENCES safe_inputs(safe_input_index),
    CHECK ((sealed_at_ms IS NULL) = (payload_hash IS NULL)),
    CHECK (accepted_input IS NULL OR (sealed_at_ms IS NOT NULL AND invalidated_at_ms IS NULL))
) STRICT;

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

-- At most one valid batch per nonce. Also the submitter's hot path ("valid
-- closed batches with nonce >= N", ordered): its predicate implies this one's.
CREATE UNIQUE INDEX IF NOT EXISTS ux_valid_batch_nonce
    ON batches(nonce)
    WHERE invalidated_at_ms IS NULL;

-- ── Views ──────────────────────────────────────────────────────────────────
-- Readers over batch data go through these `valid_*` views, which exclude
-- invalidated rows; writers always target the base tables.
CREATE VIEW IF NOT EXISTS valid_batches AS
    SELECT * FROM batches WHERE invalidated_at_ms IS NULL;

CREATE VIEW IF NOT EXISTS valid_closed_batches AS
    SELECT * FROM valid_batches WHERE sealed_at_ms IS NOT NULL;

-- At most one row by the partial unique index above.
CREATE VIEW IF NOT EXISTS valid_open_batch AS
    SELECT * FROM valid_batches WHERE sealed_at_ms IS NULL;

-- ── Triggers ───────────────────────────────────────────────────────────────
--
-- These enforce invariants the writer could otherwise violate with a bug.
-- Keep them declarative: each one names an invariant and refuses writes that
-- would break it. The Rust writer is still the source of truth for the
-- transition sequence — triggers just ensure the DB never reaches an
-- inconsistent state if the writer misbehaves.
--
-- Every table is STRICT: a value of the wrong storage class is refused for
-- every column before any trigger runs, so trigger arithmetic, frontier folds
-- and comparisons only ever see the declared class (integers where they count).

-- A batch is inserted unaccepted (acceptance is an UPDATE, which the triggers
-- below order), carrying the last valid nonce + 1, or the era's
-- `anchor_batch_nonce` (its next batch nonce at the stop block) when no batch
-- is valid. With neither the right side is NULL, `IS NOT` holds and the insert
-- aborts: no batch exists before the era. SQLite fires the most recently
-- created BEFORE trigger first, so the divergence freeze (created below) answers
-- before this one while its marker exists.
CREATE TRIGGER IF NOT EXISTS trg_batch_insert
BEFORE INSERT ON batches FOR EACH ROW
BEGIN
    SELECT CASE
        WHEN NEW.accepted_input IS NOT NULL
            THEN RAISE(ABORT, 'a batch is inserted unaccepted')
        WHEN NEW.nonce IS NOT COALESCE(
                (SELECT MAX(nonce) + 1 FROM batches WHERE invalidated_at_ms IS NULL),
                (SELECT anchor_batch_nonce FROM era WHERE singleton_id = 0))
            THEN RAISE(ABORT, 'a batch nonce is the last valid nonce + 1, or the era anchor')
    END;
END;

-- A batch row changes only by sealing once, with its hash, while valid and
-- unaccepted; by being accepted once while sealed and valid; or by being
-- invalidated once while unaccepted. SQLite fires the most recently created
-- BEFORE trigger first: the divergence freeze (created below) answers first,
-- then the acceptance order, then this one.
CREATE TRIGGER IF NOT EXISTS trg_batch_lifecycle
BEFORE UPDATE ON batches FOR EACH ROW
WHEN NOT (NEW.batch_index IS OLD.batch_index AND NEW.nonce IS OLD.nonce
      AND NEW.created_at_ms IS OLD.created_at_ms
      AND ( (OLD.sealed_at_ms IS NULL AND NEW.sealed_at_ms IS NOT NULL
             AND OLD.payload_hash IS NULL AND NEW.payload_hash IS NOT NULL
             AND OLD.invalidated_at_ms IS NULL AND NEW.invalidated_at_ms IS NULL
             AND OLD.accepted_input IS NULL AND NEW.accepted_input IS NULL)
         OR (OLD.sealed_at_ms IS NOT NULL
             AND NEW.sealed_at_ms IS OLD.sealed_at_ms AND NEW.payload_hash IS OLD.payload_hash
             AND OLD.invalidated_at_ms IS NULL AND NEW.invalidated_at_ms IS NULL
             AND OLD.accepted_input IS NULL AND NEW.accepted_input IS NOT NULL)
         OR (NEW.sealed_at_ms IS OLD.sealed_at_ms AND NEW.payload_hash IS OLD.payload_hash
             AND OLD.accepted_input IS NULL AND NEW.accepted_input IS NULL
             AND OLD.invalidated_at_ms IS NULL AND NEW.invalidated_at_ms IS NOT NULL)))
BEGIN SELECT RAISE(ABORT, 'a batch changes only by sealing once with its hash, by being accepted once while sealed, or by invalidating once while unaccepted'); END;

-- Acceptance extends the accepted prefix of the valid path: the accepted
-- batch carries the greatest accepted batch's nonce + 1 (the era anchor
-- before any acceptance) and a later input than every earlier acceptance.
-- With no accepted batch and no era the right side is NULL, `IS NOT` holds and
-- the update aborts.
CREATE TRIGGER IF NOT EXISTS trg_batch_acceptance_extends_the_prefix
BEFORE UPDATE OF accepted_input ON batches FOR EACH ROW
WHEN OLD.accepted_input IS NULL AND NEW.accepted_input IS NOT NULL AND (
     NEW.nonce IS NOT COALESCE(
         (SELECT nonce + 1 FROM batches WHERE accepted_input = (SELECT MAX(accepted_input) FROM batches)),
         (SELECT anchor_batch_nonce FROM era WHERE singleton_id = 0))
  OR NEW.accepted_input <= COALESCE((SELECT MAX(accepted_input) FROM batches), -1))
BEGIN SELECT RAISE(ABORT, 'an acceptance extends the accepted prefix of the valid path'); END;

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
) STRICT;

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

-- No (sender, nonce) uniqueness is enforced here: included user-op identity
-- is tracked by application nonce logic, and a DB constraint would block
-- legitimate resubmission after a recovery cascade.
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
) STRICT;

-- `GET /nonce` reads a sender's latest op in a valid batch. Rows are never
-- pruned, so without this index every public read would scan all history.
-- It is the chunk commit's costliest index: sender-keyed entries land on
-- scattered leaves, dirtying about one WAL page per distinct sender per chunk.
-- Measured cost and alternatives: docs/review/register.md, "Known optimizations".
CREATE INDEX IF NOT EXISTS idx_user_ops_sender_nonce
    ON user_ops(sender, nonce);

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

CREATE TABLE IF NOT EXISTS safe_inputs (
    safe_input_index INTEGER PRIMARY KEY,
    sender             BLOB NOT NULL CHECK (length(sender) = 20),
    payload            BLOB NOT NULL,
    -- Block number of the chain block where this direct input was included (e.g. InputAdded event block).
    block_number       INTEGER NOT NULL CHECK (block_number >= 0),
    -- Timestamp of the carrying L1 block.
    block_timestamp    INTEGER NOT NULL CHECK (block_timestamp >= 0),
    -- Hash of the L1 transaction that carried this input.
    transaction_hash   BLOB NOT NULL CHECK (length(transaction_hash) = 32)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_safe_inputs_sender
    ON safe_inputs(sender);

-- The InputBox record is append-only: its rows are the source of every
-- replayed direct input and of the acceptance scan (I11). Direct reads filter by
-- sender, so a changed or missing undrained row would execute silently.
CREATE TRIGGER IF NOT EXISTS trg_safe_inputs_immutable
BEFORE UPDATE ON safe_inputs FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'safe inputs are immutable'); END;
CREATE TRIGGER IF NOT EXISTS trg_safe_inputs_not_deletable
BEFORE DELETE ON safe_inputs FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'safe inputs are retained for the database lifetime'); END;

-- Current application order. Recovery replaces only an invalidated suffix;
-- immutable source payloads remain in their owning tables.
CREATE TABLE IF NOT EXISTS application_inputs (
    l2_tx_count INTEGER PRIMARY KEY CHECK (l2_tx_count >= 0),
    batch_index INTEGER NOT NULL,
    frame_in_batch INTEGER NOT NULL,
    user_op_pos_in_frame INTEGER,
    safe_input_index INTEGER,
    FOREIGN KEY(batch_index, frame_in_batch)
        REFERENCES frames(batch_index, frame_in_batch),
    FOREIGN KEY(batch_index, frame_in_batch, user_op_pos_in_frame)
        REFERENCES user_ops(batch_index, frame_in_batch, pos_in_frame),
    FOREIGN KEY(safe_input_index) REFERENCES safe_inputs(safe_input_index),
    CHECK ((user_op_pos_in_frame IS NOT NULL AND safe_input_index IS NULL)
        OR (user_op_pos_in_frame IS NULL AND safe_input_index IS NOT NULL)),
    UNIQUE(batch_index, frame_in_batch, user_op_pos_in_frame),
    UNIQUE(safe_input_index)
) STRICT;
CREATE INDEX IF NOT EXISTS idx_application_inputs_frame
    ON application_inputs(batch_index, frame_in_batch);
CREATE INDEX IF NOT EXISTS idx_safe_inputs_block
    ON safe_inputs(block_number, safe_input_index);

CREATE TABLE IF NOT EXISTS l1_safe_head (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    -- Highest L1 safe block the input reader has observed and atomically synced into storage.
    block_number INTEGER NOT NULL CHECK (block_number >= 0),
    -- L1 timestamp (Unix seconds) of block_number.
    block_timestamp INTEGER NOT NULL CHECK (block_timestamp >= 0),
    -- Wall-clock time (Unix ms) of the last successful L1 sync.
    -- Used for wall-clock danger estimation when L1 is unreachable.
    synced_at_ms INTEGER NOT NULL CHECK (synced_at_ms >= 0)
) STRICT;

-- Highest wallet nonce ever broadcast by this deployment's batch-submitter
-- key (the durable realization of the TLA+ spec's `walletNonce`). Write-before-broadcast: any component about to send a tx
-- at wallet nonce n first commits watermark = max(watermark, n) — power-loss
-- durable under synchronous=FULL — then sends. Uniform for batch txs and
-- flush no-ops alike, so the flush's slot coverage never depends on the
-- local node's volatile mempool memory (which a dropped-locally but
-- network-alive zombie tx evades). Absent row =
-- nothing ever broadcast. Never reset, never lowered.
CREATE TABLE IF NOT EXISTS wallet_nonce_watermark (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    watermark    INTEGER NOT NULL CHECK (watermark >= 0)
) STRICT;

-- Canonical-divergence poison marker. Written by the input
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
) STRICT;

-- I15 structural enforcement: while the divergence marker exists, the batch
-- tree, its accepted frontier and snapshot collection are frozen in the engine
-- itself. Standard recovery is forbidden on a diverged frontier; the typed
-- Rust refusals (the local-first startup recovery procedure plus guarded Tip/Cascade
-- mutations and atomic runtime admission) remain the friendly error surface, but these
-- triggers are the enforcement a forgotten call site cannot bypass. Created
-- after every other BEFORE trigger on `batches`, so they answer first.
CREATE TRIGGER IF NOT EXISTS trg_batches_frozen_on_divergence_insert
BEFORE INSERT ON batches FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM canonical_divergence WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'batch tree frozen: canonical divergence marker present'); END;

CREATE TRIGGER IF NOT EXISTS trg_batches_frozen_on_divergence_update
BEFORE UPDATE ON batches FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM canonical_divergence WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'batch tree frozen: canonical divergence marker present'); END;

-- The history setup rebuilds from L1, from its base through the stop block:
-- one row per transaction that executed, keyed by its L2 tx count. A direct
-- keeps only its input index (safe_inputs holds the rest); a user op keeps what
-- executed. `batch_nonce` is the scheduler's next batch nonce when it handed
-- the row out. A user op the executor skipped has no row. Only setup writes
-- here, and only before it publishes the era.
CREATE TABLE IF NOT EXISTS rebuilt_history (
    l2_tx_count      INTEGER PRIMARY KEY CHECK (l2_tx_count >= 0),
    batch_nonce      INTEGER NOT NULL CHECK (batch_nonce >= 0),
    safe_input_index INTEGER UNIQUE REFERENCES safe_inputs(safe_input_index),
    sender           BLOB    CHECK (sender IS NULL OR length(sender) = 20),
    nonce            INTEGER CHECK (nonce IS NULL OR nonce BETWEEN 0 AND 4294967295),
    fee              INTEGER CHECK (fee IS NULL OR fee BETWEEN 0 AND 65535),
    safe_block       INTEGER CHECK (safe_block IS NULL OR safe_block >= 0),
    data             BLOB,
    CHECK ((safe_input_index IS NOT NULL AND sender IS NULL AND nonce IS NULL AND fee IS NULL
            AND safe_block IS NULL AND data IS NULL)
        OR (safe_input_index IS NULL AND sender IS NOT NULL AND nonce IS NOT NULL AND fee IS NOT NULL
            AND safe_block IS NOT NULL AND data IS NOT NULL))
) STRICT;

-- Rows are inserted in execution order until the era is published. The key
-- is a rowid alias: an insert without a count would be numbered by SQLite,
-- and a BEFORE trigger sees that count as -1. The first row may carry any
-- count; `trg_era_fits` ties it to the base count. A direct's input index
-- rises row over row because the scheduler's queue is FIFO; with no earlier
-- direct the MAX is NULL, the comparison is NULL and the row passes.
CREATE TRIGGER IF NOT EXISTS trg_rebuilt_history_insert
BEFORE INSERT ON rebuilt_history FOR EACH ROW
BEGIN
    SELECT CASE
        WHEN EXISTS (SELECT 1 FROM era)
            THEN RAISE(ABORT, 'rebuilt history is sealed once its era is published')
        WHEN NEW.l2_tx_count IS NULL OR NEW.l2_tx_count < 0
            THEN RAISE(ABORT, 'a rebuilt row carries its L2 tx count')
        WHEN EXISTS (SELECT 1 FROM rebuilt_history)
         AND NEW.l2_tx_count IS NOT (SELECT MAX(l2_tx_count) + 1 FROM rebuilt_history)
            THEN RAISE(ABORT, 'rebuilt counts follow one another without gaps')
        WHEN NEW.safe_input_index IS NOT NULL
         AND (SELECT sender FROM safe_inputs WHERE safe_input_index = NEW.safe_input_index)
           = (SELECT batch_submitter_address FROM deployment_identity WHERE singleton_id = 0)
            THEN RAISE(ABORT, 'a batch submitter input never executes as a direct')
        WHEN NEW.safe_input_index IS NOT NULL
         AND NEW.safe_input_index <= (SELECT MAX(safe_input_index) FROM rebuilt_history)
            THEN RAISE(ABORT, 'rebuilt directs drain in input order')
    END;
END;
CREATE TRIGGER IF NOT EXISTS trg_rebuilt_history_immutable
BEFORE UPDATE ON rebuilt_history FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'rebuilt history rows are immutable'); END;
CREATE TRIGGER IF NOT EXISTS trg_rebuilt_history_not_deletable
BEFORE DELETE ON rebuilt_history FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'rebuilt history is retained for the database lifetime'); END;

-- The era: one immutable row, written by setup's publish. Its history starts
-- at the base count; setup's rebuild covers [base_l2_tx_count,
-- rebuilt_history_end) and the live rows follow. The lane drains directs from
-- `first_queued_input` until a frame of the era accounts for more.
-- `anchor_batch_nonce` is the nonce a batch carries when no batch is valid
-- (I16). A database is set up exactly when this row exists.
CREATE TABLE IF NOT EXISTS era (
    singleton_id        INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    era_id              BLOB    NOT NULL CHECK (
        length(era_id) = 16
        AND substr(hex(era_id), 13, 1) = '4'
        AND substr(hex(era_id), 17, 1) IN ('8', '9', 'A', 'B')),
    published_at_ms     INTEGER NOT NULL CHECK (published_at_ms >= 0),
    base_l2_tx_count    INTEGER NOT NULL CHECK (base_l2_tx_count >= 0),
    rebuilt_history_end INTEGER NOT NULL CHECK (rebuilt_history_end >= base_l2_tx_count),
    stop_block          INTEGER NOT NULL CHECK (stop_block >= 0),
    first_queued_input  INTEGER NOT NULL CHECK (first_queued_input >= 0),
    anchor_batch_nonce  INTEGER NOT NULL CHECK (anchor_batch_nonce >= 0)
) STRICT;
-- The era's counts are exactly the span of the rebuilt rows, and its first
-- queued input lies among the inputs through the stop block. The scheduler
-- drains its queue in input order, so every rebuilt direct lies before the
-- first one still queued: a queued one would run twice.
CREATE TRIGGER IF NOT EXISTS trg_era_fits
BEFORE INSERT ON era FOR EACH ROW
BEGIN
    SELECT CASE
        WHEN NEW.base_l2_tx_count != COALESCE(
                (SELECT MIN(l2_tx_count) FROM rebuilt_history), NEW.base_l2_tx_count)
          OR NEW.rebuilt_history_end != COALESCE(
                (SELECT MAX(l2_tx_count) + 1 FROM rebuilt_history), NEW.base_l2_tx_count)
            THEN RAISE(ABORT, 'the era must span exactly its executed rebuilt rows')
        WHEN EXISTS (SELECT 1 FROM rebuilt_history
                      WHERE safe_input_index IS NOT NULL
                        AND safe_input_index >= NEW.first_queued_input)
            THEN RAISE(ABORT, 'a rebuilt direct is never still queued')
        WHEN NEW.first_queued_input > COALESCE(
                (SELECT safe_input_index + 1 FROM safe_inputs
                  WHERE block_number <= NEW.stop_block
                  ORDER BY block_number DESC, safe_input_index DESC LIMIT 1), 0)
            THEN RAISE(ABORT, 'the first queued input lies past the stop block')
    END;
END;
-- `INSERT OR REPLACE` deletes the old row without firing DELETE triggers
-- (`recursive_triggers` is off), so only this BEFORE INSERT trigger stops it.
CREATE TRIGGER IF NOT EXISTS trg_era_single_insert
BEFORE INSERT ON era FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM era)
BEGIN SELECT RAISE(ABORT, 'the era is inserted once per database'); END;
CREATE TRIGGER IF NOT EXISTS trg_era_immutable
BEFORE UPDATE ON era FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'the era is immutable'); END;
CREATE TRIGGER IF NOT EXISTS trg_era_not_deletable
BEFORE DELETE ON era FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'the era is retained for the database lifetime'); END;
-- One cut for each recovery transition, measured after invalidation removes
-- the old suffix and before reopening attributes replacement direct inputs.
-- The recovery generation is the greatest cut's (0 with none): there is no
-- stored counter, so this ledger is the generation's only home.
CREATE TABLE IF NOT EXISTS history_generation_cuts (
    recovery_generation INTEGER PRIMARY KEY CHECK (recovery_generation > 0),
    preserved_l2_tx_count INTEGER NOT NULL CHECK (preserved_l2_tx_count >= 0)
) STRICT;
-- With no era the subquery yields no row, `IS NOT` compares with NULL and
-- holds, and the insert aborts: a cut never exists without its era.
CREATE TRIGGER IF NOT EXISTS trg_history_generation_cuts_contiguous
BEFORE INSERT ON history_generation_cuts FOR EACH ROW
WHEN NEW.recovery_generation IS NOT (
    SELECT COALESCE((SELECT MAX(recovery_generation) FROM history_generation_cuts), 0) + 1
      FROM era WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'a cut advances the recovery generation by exactly one, within an era'); END;
CREATE TRIGGER IF NOT EXISTS trg_history_generation_cuts_immutable
BEFORE UPDATE ON history_generation_cuts FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'history generation cuts are immutable'); END;
CREATE TRIGGER IF NOT EXISTS trg_history_generation_cuts_not_deletable
BEFORE DELETE ON history_generation_cuts FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'history generation cuts are retained throughout the era'); END;

CREATE TRIGGER IF NOT EXISTS trg_application_inputs_target_must_be_tip
BEFORE INSERT ON application_inputs FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM batches WHERE batch_index = NEW.batch_index
    AND sealed_at_ms IS NULL AND invalidated_at_ms IS NULL)
BEGIN SELECT RAISE(ABORT, 'application input must target the current valid Tip'); END;
-- Live rows continue the era's rebuilt history. With no era the subquery is
-- NULL, `IS NOT` holds and the insert aborts.
CREATE TRIGGER IF NOT EXISTS trg_application_inputs_contiguous
BEFORE INSERT ON application_inputs FOR EACH ROW
WHEN NEW.l2_tx_count IS NOT (SELECT MAX(rebuilt_history_end,
    COALESCE((SELECT MAX(l2_tx_count) + 1 FROM application_inputs), 0))
    FROM era WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'application input offset must equal next count'); END;
-- The same refusal with a clearer message. SQLite fires the most recently
-- created trigger first, so this one, created after the arm above, speaks.
CREATE TRIGGER IF NOT EXISTS trg_application_inputs_requires_baseline
BEFORE INSERT ON application_inputs FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM era)
BEGIN SELECT RAISE(ABORT, 'application history baseline is missing'); END;
CREATE TRIGGER IF NOT EXISTS trg_application_inputs_immutable
BEFORE UPDATE ON application_inputs FOR EACH ROW
BEGIN SELECT RAISE(ABORT, 'application inputs are immutable'); END;
CREATE TRIGGER IF NOT EXISTS trg_protect_valid_application_input_delete
BEFORE DELETE ON application_inputs FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM batches WHERE batch_index = OLD.batch_index
    AND invalidated_at_ms IS NULL)
BEGIN SELECT RAISE(ABORT, 'valid application input cannot be deleted'); END;
CREATE TRIGGER IF NOT EXISTS trg_drop_invalidated_application_inputs
AFTER UPDATE OF invalidated_at_ms ON batches FOR EACH ROW
WHEN OLD.invalidated_at_ms IS NULL AND NEW.invalidated_at_ms IS NOT NULL
BEGIN DELETE FROM application_inputs WHERE batch_index = NEW.batch_index; END;

-- Terminal-fault black box: an append-only trail of terminal causes,
-- best-effort recorded before death. DELIBERATELY NOT AN ADMISSION GATE:
-- admission
-- is governed by facts — the kernel process lock excludes concurrent
-- owners, the era's existence orders commands (two-sided), and
-- `canonical_divergence` is the one absorbing refusal (cockroach rebuild is
-- the only exit). Restart policy after a terminal fault is the exit-code
-- contract (30 = do not restart, page), not a database gate. Nothing reads
-- this table for decisions; it exists so the cause of death travels with
-- the data directory for operator postmortems, surviving log rotation.
CREATE TABLE IF NOT EXISTS terminal_faults (
    fault_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    command        TEXT    NOT NULL CHECK (command IN (
        'setup', 'run', 'maintenance_flush'
    )),
    cause          TEXT    NOT NULL CHECK (length(cause) > 0),
    recorded_at_ms INTEGER NOT NULL CHECK (recorded_at_ms >= 0)
) STRICT;

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
    -- The longest direct payload the application executes; with the batch
    -- submitter address it decides which safe inputs execute as directs.
    max_direct_payload_bytes  INTEGER NOT NULL CHECK (max_direct_payload_bytes >= 0),
    -- The application's staleness window in L1 blocks: a batch whose first
    -- frame is this old when it lands is skipped, so acceptance is judged by it.
    max_wait_blocks           INTEGER NOT NULL CHECK (max_wait_blocks >= 1),
    -- Immutable setup-pinned fee source. Fixed has only fixed_log_gas_price;
    -- Uniswap has the complete reviewed token/pool tuple.
    fee_oracle_mode           TEXT    NOT NULL CHECK (fee_oracle_mode IN ('fixed', 'uniswap')),
    fixed_log_gas_price       INTEGER CHECK (fixed_log_gas_price BETWEEN -32768 AND 32767),
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
) STRICT;

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
--   WETH/X TWAP and encodes the approximate quote to log space. The tenfold safety
--   margin lives in `log_slack` (not in the oracle). Local Anvil uses an
--   explicit fixed exponent instead of Uniswap.
--   `log_gas_price` may be negative: a few-decimal fee token (USDC) can cost
--   less than one smallest unit per gas at low L1 gas. The floor is set on
--   the derived fee instead, one smallest unit per user op (exponent 0);
--   below it the oracle clamps rather than overcharging per gas.
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
--   InputBox gas ≈ base_gas + delta × payload bytes; each op pays
--   (1 + alpha) × delta per byte, so alpha × delta × batch_size_target = base_gas.
--
-- Batch size invariant (CHECK):
--   log_base_gas - log_alpha - log_delta < log_max_batch_bytes
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS batch_policy (
    singleton_id             INTEGER PRIMARY KEY CHECK (singleton_id = 0),

    -- Knobs (schema defaults; no runtime writer; tests override by SQL):
    -- log_alpha = log(alpha) where alpha = num/denom. Can be negative (alpha < 1).
    log_alpha                INTEGER NOT NULL,
    -- log_one_plus_alpha = log(1 + alpha). Always >= 0.
    log_one_plus_alpha       INTEGER NOT NULL CHECK (log_one_plus_alpha >= 0),
    -- Log-space fee exponent fed by the oracle. Negative below one fee-token
    -- smallest unit per L1 gas; the fee-floor CHECK below bounds it.
    log_gas_price            INTEGER NOT NULL,
    -- Unix-ms of the last successful oracle (or Fixed setup) write.
    -- 0 means never written; completed setup guarantees a nonzero observation.
    log_gas_price_updated_at_ms INTEGER NOT NULL CHECK (log_gas_price_updated_at_ms >= 0),
    -- log_{129/128}(10) = 296: the tenfold price slack (see the header).
    log_slack                INTEGER NOT NULL CHECK (log_slack >= 0),

    -- Constants (log-space):
    log_base_gas             INTEGER NOT NULL CHECK (log_base_gas > 0),
    log_delta                INTEGER NOT NULL CHECK (log_delta > 0),
    log_user_op_bytes        INTEGER NOT NULL CHECK (log_user_op_bytes > 0),
    log_max_batch_bytes      INTEGER NOT NULL CHECK (log_max_batch_bytes > 0),

    CHECK (log_base_gas - log_alpha - log_delta < log_max_batch_bytes),
    -- Fee floor: the derived per-op fee never drops below exponent 0.
    CHECK (log_gas_price + log_slack + log_one_plus_alpha + log_delta + log_user_op_bytes >= 0)
) STRICT;

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
    -- Fee per user op.
    log_gas_price + log_slack + log_one_plus_alpha + log_delta + log_user_op_bytes
        AS log_recommended_fee,
    -- Batch size target in log-space (convert via fee_to_linear for bytes).
    log_base_gas - log_alpha - log_delta
        AS log_batch_size_target
FROM batch_policy;

-- ---------------------------------------------------------------------------
-- Snapshots
--
-- A snapshot: an immutable artifact directory and the boundary it holds — one
-- local batch at its close, or the era baseline (batch_index NULL). Files are
-- durable before the row is inserted; rows are deleted before files.
-- Acceptance is read from `batches.accepted_input`; it never mutates snapshots.
-- Download leases live in the serving process, not here
-- (docs/snapshots/lifecycle.md).
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS snapshots (
    snapshot_id INTEGER PRIMARY KEY,
    batch_index INTEGER UNIQUE REFERENCES batches(batch_index) ON DELETE RESTRICT,
    l2_tx_count INTEGER NOT NULL CHECK (l2_tx_count >= 0),
    prefix      TEXT    NOT NULL UNIQUE
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS snapshots_single_baseline
ON snapshots ((1)) WHERE batch_index IS NULL;

CREATE TRIGGER IF NOT EXISTS trg_snapshot_immutable
BEFORE UPDATE ON snapshots
BEGIN SELECT RAISE(ABORT, 'snapshot boundaries are immutable'); END;

CREATE TRIGGER IF NOT EXISTS trg_snapshot_clear_frozen_on_divergence
BEFORE DELETE ON snapshots FOR EACH ROW
WHEN EXISTS (SELECT 1 FROM canonical_divergence WHERE singleton_id = 0)
BEGIN SELECT RAISE(ABORT, 'snapshot collection frozen: canonical divergence marker present'); END;
