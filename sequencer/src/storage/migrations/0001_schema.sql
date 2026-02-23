CREATE TABLE IF NOT EXISTS batches (
    batch_index    INTEGER PRIMARY KEY,
    created_at_ms  INTEGER NOT NULL,
    -- Fee committed by the sequencer for this whole batch.
    fee            INTEGER NOT NULL CHECK (fee >= 0)
);

CREATE TABLE IF NOT EXISTS frames (
    batch_index          INTEGER NOT NULL REFERENCES batches(batch_index),
    frame_in_batch       INTEGER NOT NULL,
    created_at_ms        INTEGER NOT NULL,
    PRIMARY KEY(batch_index, frame_in_batch)
);

CREATE TABLE IF NOT EXISTS user_ops (
    batch_index      INTEGER NOT NULL,
    frame_in_batch   INTEGER NOT NULL,
    pos_in_frame     INTEGER NOT NULL,
    tx_hash          BLOB NOT NULL,
    sender           BLOB NOT NULL,
    nonce            INTEGER NOT NULL,
    max_fee          INTEGER NOT NULL,
    data             BLOB NOT NULL,
    sig              BLOB NOT NULL,
    received_at_ms   INTEGER NOT NULL,
    PRIMARY KEY(batch_index, frame_in_batch, pos_in_frame),
    FOREIGN KEY(batch_index, frame_in_batch) REFERENCES frames(batch_index, frame_in_batch),
    UNIQUE(tx_hash)
);

CREATE TABLE IF NOT EXISTS direct_inputs (
    direct_input_index INTEGER PRIMARY KEY,
    payload            BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS frame_drains (
    batch_index        INTEGER NOT NULL,
    frame_in_batch     INTEGER NOT NULL,
    drain_n            INTEGER NOT NULL,
    PRIMARY KEY(batch_index, frame_in_batch),
    FOREIGN KEY(batch_index, frame_in_batch) REFERENCES frames(batch_index, frame_in_batch),
    CHECK(drain_n >= 0)
);

CREATE INDEX IF NOT EXISTS idx_frames_batch_order
    ON frames(batch_index, frame_in_batch);
CREATE INDEX IF NOT EXISTS idx_user_ops_frame_pos
    ON user_ops(batch_index, frame_in_batch, pos_in_frame);
CREATE INDEX IF NOT EXISTS idx_frame_drains_frame
    ON frame_drains(batch_index, frame_in_batch);

CREATE TABLE IF NOT EXISTS recommended_fees (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    -- Mutable recommendation consumed when opening the next batch.
    fee          INTEGER NOT NULL CHECK (fee >= 0)
);

INSERT OR IGNORE INTO recommended_fees (singleton_id, fee)
VALUES (0, 1);

INSERT OR IGNORE INTO batches (batch_index, created_at_ms, fee)
VALUES (0, 0, 1);

INSERT OR IGNORE INTO frames (batch_index, frame_in_batch, created_at_ms)
VALUES (0, 0, 0);
