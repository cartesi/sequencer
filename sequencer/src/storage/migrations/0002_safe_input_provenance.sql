-- L1 inclusion provenance for direct inputs, projected into the WS feed so a
-- downstream mirror can log a direct's carrying transaction and block time
-- without its own L1 view. Backfilled as zero for pre-migration rows (new
-- deployments always start from scratch).
ALTER TABLE safe_inputs ADD COLUMN block_timestamp INTEGER NOT NULL DEFAULT 0 CHECK (block_timestamp >= 0);
ALTER TABLE safe_inputs ADD COLUMN transaction_hash BLOB NOT NULL
    DEFAULT X'0000000000000000000000000000000000000000000000000000000000000000'
    CHECK (length(transaction_hash) = 32);
