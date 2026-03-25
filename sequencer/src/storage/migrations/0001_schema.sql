CREATE TABLE IF NOT EXISTS batches (
    batch_index    INTEGER PRIMARY KEY,
    created_at_ms  INTEGER NOT NULL
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
    FOREIGN KEY(batch_index, frame_in_batch) REFERENCES frames(batch_index, frame_in_batch),
    UNIQUE(sender, nonce)
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
    UNIQUE(batch_index, frame_in_batch, user_op_pos_in_frame),
    -- A direct input can only be sequenced once.
    UNIQUE(safe_input_index)
);

CREATE INDEX IF NOT EXISTS idx_sequenced_l2_txs_frame
    ON sequenced_l2_txs(batch_index, frame_in_batch);

CREATE TABLE IF NOT EXISTS l1_safe_head (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    -- Highest L1 safe block the input reader has observed and atomically synced into storage.
    block_number INTEGER NOT NULL CHECK (block_number >= 0)
);

INSERT OR IGNORE INTO l1_safe_head (singleton_id, block_number)
VALUES (0, 0);

-- ---------------------------------------------------------------------------
-- Batch policy singleton
--
-- Contains operator-tunable knobs (alpha, gas_price) and on-chain constants
-- (delta, base_gas, etc.). A view derives `batch_size_target` and
-- `recommended_fee` from these columns, and a CHECK constraint prevents
-- updates that would violate the batch size limit.
--
-- Gas economics:
--   batch_size_target = const_base_gas * alpha_denom / (alpha_num * const_delta)
--   recommended_fee   = gas_price * (alpha_num + alpha_denom)
--                        * const_delta * const_user_op_bytes / alpha_denom
--
-- Fee unit:
--   `gas_price` is denominated in "L2 smallest-token-unit per L1 gas unit".
--   The entity feeding this value (e.g. a scheduler/price-oracle) must
--   convert the L1 gas price in wei and the L1↔L2 exchange rate into this
--   single number.  For tokens with few decimals (e.g. USDC, 6 decimals)
--   the scheduler should pre-scale the value (multiply by 10^k) so that
--   sub-unit precision is not lost to integer truncation.
--
-- Overflow safety:
--   The intermediate product in `recommended_fee` is:
--     gas_price * (alpha_num + alpha_denom) * const_delta * const_user_op_bytes
--   With the current constants this equals gas_price × 1168 × 26 × 126
--   = gas_price × 3,826,368.  SQLite uses signed 64-bit integers and
--   silently wraps on overflow (no detection mechanism).  The CHECK on
--   gas_price caps it at 2 × 10^12, keeping the intermediate product
--   well below i64::MAX ≈ 9.2 × 10^18.  The Rust reader additionally
--   validates with checked arithmetic.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS batch_policy (
    singleton_id             INTEGER PRIMARY KEY CHECK (singleton_id = 0),

    -- Knobs (operator-tunable via sqlite3 CLI):
    alpha_num                INTEGER NOT NULL CHECK (alpha_num > 0),
    alpha_denom              INTEGER NOT NULL CHECK (alpha_denom > 0),
    -- See "Fee unit" and "Overflow safety" in the comment block above.
    gas_price                INTEGER NOT NULL CHECK (gas_price >= 0 AND gas_price <= 2000000000000),

    -- Constants (in DB so the CHECK can reference them):
    const_delta              INTEGER NOT NULL CHECK (const_delta > 0),
    const_base_gas           INTEGER NOT NULL CHECK (const_base_gas > 0),
    const_user_op_bytes      INTEGER NOT NULL CHECK (const_user_op_bytes > 0),
    -- Effective max batch payload. Already includes slack for chunk overshoot
    -- and SSZ framing, so the CHECK is simply batch_size_target < this value.
    const_max_batch_bytes    INTEGER NOT NULL CHECK (const_max_batch_bytes > 0),

    -- Safety: batch_size_target < const_max_batch_bytes.
    CHECK (
        const_base_gas * alpha_denom / (alpha_num * const_delta)
        < const_max_batch_bytes
    )
);

INSERT OR IGNORE INTO batch_policy(
    singleton_id,

    alpha_num, alpha_denom, gas_price,

    const_delta, const_base_gas,
    const_user_op_bytes, const_max_batch_bytes
)
VALUES (
    -- Fixed id
    0,

    -- Knobs
    168, 1000, 0,

    -- Constants
    26, 55000,
    126, 32000
);

-- Derived view for reads.
CREATE VIEW IF NOT EXISTS batch_policy_derived AS
SELECT *,
    const_base_gas * alpha_denom / (alpha_num * const_delta)
        AS batch_size_target,

    gas_price * (alpha_num + alpha_denom) * const_delta * const_user_op_bytes / alpha_denom
        AS recommended_fee
FROM batch_policy;
