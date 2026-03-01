-- Input reader cursor: last block from which safe inputs have been read.
-- Used to resume chain sync on restart.
CREATE TABLE IF NOT EXISTS input_reader_state (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 0),
    last_processed_block INTEGER NOT NULL CHECK (last_processed_block >= 0)
);

INSERT OR IGNORE INTO input_reader_state (singleton_id, last_processed_block)
VALUES (0, 0);
