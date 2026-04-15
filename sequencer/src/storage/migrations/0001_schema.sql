CREATE TABLE IF NOT EXISTS batches (
    batch_index    INTEGER PRIMARY KEY,
    created_at_ms  INTEGER NOT NULL
);

-- Batches that missed their submission deadline and will never be executed
-- by the scheduler. Append-only: once a batch is marked invalid it stays invalid.
-- The sequencer recovery procedure populates this table at startup.
-- Cascading: if batch B is invalid, all batches with batch_index > B are also invalid.
CREATE TABLE IF NOT EXISTS invalid_batches (
    batch_index INTEGER PRIMARY KEY REFERENCES batches(batch_index)
);

-- Nonce assignments for batches. Populated by the batch submitter.
-- Nonces are assigned to valid batches in order. After cascading invalidation,
-- new batches reuse nonces (nonces are NOT unique across the table).
CREATE TABLE IF NOT EXISTS batch_nonces (
    batch_index INTEGER PRIMARY KEY REFERENCES batches(batch_index),
    nonce       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_batch_nonces_nonce_batch
    ON batch_nonces(nonce, batch_index);

-- ---------------------------------------------------------------------------
-- Valid-row views
--
-- Application-level reads almost always exclude rows from invalidated batches.
-- These views encapsulate that filter so individual queries don't have to
-- repeat `WHERE batch_index NOT IN (SELECT batch_index FROM invalid_batches)`.
--
-- Writers go to the base tables. Readers go through the views unless they
-- explicitly need to see invalid rows (e.g., the cascade-collection query
-- inside `recovery::detect_stale_and_collect_cascade`).
-- ---------------------------------------------------------------------------
CREATE VIEW IF NOT EXISTS valid_batches AS
SELECT * FROM batches
WHERE batch_index NOT IN (SELECT batch_index FROM invalid_batches);

CREATE VIEW IF NOT EXISTS valid_batch_nonces AS
SELECT * FROM batch_nonces
WHERE batch_index NOT IN (SELECT batch_index FROM invalid_batches);

-- Derived log of batch submissions the scheduler would actually execute.
-- Unlike a raw log of all safe submissions, this only contains the accepted
-- prefix: batches whose nonce matched the expected sequence and were not stale.
-- Populated by populate_safe_accepted_batches() which simulates the scheduler's
-- acceptance logic over safe_inputs.
CREATE TABLE IF NOT EXISTS safe_accepted_batches (
    safe_input_index     INTEGER PRIMARY KEY REFERENCES safe_inputs(safe_input_index),
    nonce                INTEGER NOT NULL,
    first_frame_safe_block INTEGER NOT NULL,
    inclusion_block      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS frames (
    batch_index          INTEGER NOT NULL REFERENCES batches(batch_index),
    frame_in_batch       INTEGER NOT NULL,
    created_at_ms        INTEGER NOT NULL,
    -- Fee committed by the sequencer for this whole frame.
    fee                  INTEGER NOT NULL CHECK (fee >= 0),
    -- Claimed safe L1 block frontier for this frame.
    safe_block           INTEGER NOT NULL CHECK (safe_block >= 0),
    PRIMARY KEY(batch_index, frame_in_batch)
);

CREATE TABLE IF NOT EXISTS user_ops (
    batch_index      INTEGER NOT NULL,
    frame_in_batch   INTEGER NOT NULL,
    pos_in_frame     INTEGER NOT NULL,
    sender           BLOB NOT NULL,
    nonce            INTEGER NOT NULL,
    max_fee          INTEGER NOT NULL,
    data             BLOB NOT NULL,
    sig              BLOB NOT NULL,
    received_at_ms   INTEGER NOT NULL,
    PRIMARY KEY(batch_index, frame_in_batch, pos_in_frame),
    FOREIGN KEY(batch_index, frame_in_batch) REFERENCES frames(batch_index, frame_in_batch)
);

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
    block_number       INTEGER NOT NULL CHECK (block_number >= 0)
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

CREATE INDEX IF NOT EXISTS idx_sequenced_l2_txs_frame
    ON sequenced_l2_txs(batch_index, frame_in_batch);

-- Partial index for efficient MAX(safe_input_index) lookups used to compute
-- the next undrained direct-input cursor at frame-close time.
CREATE INDEX IF NOT EXISTS idx_sequenced_l2_txs_safe_input
    ON sequenced_l2_txs(safe_input_index) WHERE safe_input_index IS NOT NULL;

-- See the "Valid-row views" comment above invalid_batches for the rationale.
CREATE VIEW IF NOT EXISTS valid_sequenced_l2_txs AS
SELECT * FROM sequenced_l2_txs
WHERE batch_index NOT IN (SELECT batch_index FROM invalid_batches);

CREATE TABLE IF NOT EXISTS l1_safe_head (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    -- Highest L1 safe block the input reader has observed and atomically synced into storage.
    block_number INTEGER NOT NULL CHECK (block_number >= 0),
    -- Wall-clock time (Unix ms) of the last successful L1 sync.
    -- Used for wall-clock danger estimation when L1 is unreachable.
    synced_at_ms INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO l1_safe_head (singleton_id, block_number, synced_at_ms)
VALUES (0, 0, 0);

-- L1 bootstrap cache: discovered addresses and block numbers from on-chain contracts.
-- Allows the sequencer to start without L1 if it has run before.
CREATE TABLE IF NOT EXISTS l1_bootstrap_cache (
    singleton_id       INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    input_box_address  BLOB    NOT NULL,
    genesis_block      INTEGER NOT NULL,
    chain_id           INTEGER NOT NULL
);


-- ---------------------------------------------------------------------------
-- Batch policy singleton
--
-- Every value is a log-space exponent with base 129/128 (see sequencer_core::fee).
-- Exponent N represents a linear value of (129/128)^N.
--
-- Fee unit:
--   `gas_price` is denominated in "L2 smallest-token-unit per L1 gas unit".
--   The entity feeding this value (e.g. a scheduler/price-oracle) must
--   convert the L1 gas price in wei and the L1↔L2 exchange rate into this
--   single number.
--
-- Fee derivation (view):
--   log_recommended_fee = log_gas_price + log_one_plus_alpha + log_delta + log_user_op_bytes
--   Pure addition — no overflow possible. The oracle feeds log_gas_price
--   directly in log space.
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
--   log_recommended_fee  = 0 + 20 + 419 + 621            = 1060
--   log_batch_size_target = 1403 - (-229) - 419           = 1213
INSERT OR IGNORE INTO batch_policy(
    singleton_id,

    log_alpha, log_one_plus_alpha, log_gas_price,

    log_base_gas, log_delta, log_user_op_bytes, log_max_batch_bytes
)
VALUES (
    0,

    -229, 20, 0,

    1403, 419, 621, 1333
);

-- Derived view for reads. All outputs are log-space exponents (base 129/128).
CREATE VIEW IF NOT EXISTS batch_policy_derived AS
SELECT *,
    -- Fee per user-op byte.
    log_gas_price + log_one_plus_alpha + log_delta + log_user_op_bytes
        AS log_recommended_fee,
    -- Batch size target in log-space (convert via fee_to_linear for bytes).
    log_base_gas - log_alpha - log_delta
        AS log_batch_size_target
FROM batch_policy;
