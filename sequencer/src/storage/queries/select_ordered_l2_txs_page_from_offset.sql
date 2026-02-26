SELECT kind, sender, data, fee, payload
FROM ordered_sequenced_l2_txs
-- `kind ASC` guarantees user_ops (0) are replayed before drained directs (1) in each frame.
ORDER BY batch_index ASC, frame_in_batch ASC, kind ASC, pos ASC
LIMIT ?2 OFFSET ?1
