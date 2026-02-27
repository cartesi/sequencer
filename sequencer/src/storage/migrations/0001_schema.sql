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

CREATE TABLE IF NOT EXISTS direct_inputs (
    direct_input_index INTEGER PRIMARY KEY,
    payload            BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS sequenced_l2_txs (
    -- Global append-only replay order consumed by catch-up and broadcaster.
    offset               INTEGER PRIMARY KEY,
    batch_index          INTEGER NOT NULL,
    frame_in_batch       INTEGER NOT NULL,

    -- User-op branch: references user_ops(..., pos_in_frame).
    user_op_pos_in_frame INTEGER,

    -- Direct-input branch: references direct_inputs(direct_input_index).
    direct_input_index   INTEGER,

    FOREIGN KEY(batch_index, frame_in_batch)
        REFERENCES frames(batch_index, frame_in_batch),
    FOREIGN KEY(batch_index, frame_in_batch, user_op_pos_in_frame)
        REFERENCES user_ops(batch_index, frame_in_batch, pos_in_frame),
    FOREIGN KEY(direct_input_index)
        REFERENCES direct_inputs(direct_input_index),

    -- XOR invariant: row is either a sequenced user-op OR a drained direct input.
    CHECK (
        (user_op_pos_in_frame IS NOT NULL AND direct_input_index IS NULL) OR
        (user_op_pos_in_frame IS NULL AND direct_input_index IS NOT NULL)
    ),

    -- At most one sequenced user-op row for each user-op key.
    UNIQUE(batch_index, frame_in_batch, user_op_pos_in_frame),
    -- A direct input can only be sequenced once.
    UNIQUE(direct_input_index)
);

CREATE INDEX IF NOT EXISTS idx_sequenced_l2_txs_frame
    ON sequenced_l2_txs(batch_index, frame_in_batch);

CREATE TABLE IF NOT EXISTS recommended_fees (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    -- Mutable recommendation consumed when opening the next frame.
    fee          INTEGER NOT NULL CHECK (fee >= 0)
);

INSERT OR IGNORE INTO recommended_fees (singleton_id, fee)
VALUES (0, 0);

INSERT OR IGNORE INTO batches (batch_index, created_at_ms)
VALUES (0, 0);

INSERT OR IGNORE INTO frames (batch_index, frame_in_batch, created_at_ms, fee)
VALUES (0, 0, 0, 0);
