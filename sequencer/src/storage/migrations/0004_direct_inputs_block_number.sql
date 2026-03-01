-- Block number of the chain block where each direct input was included (e.g. InputAdded event block).
ALTER TABLE direct_inputs ADD COLUMN block_number INTEGER NOT NULL DEFAULT 0;
