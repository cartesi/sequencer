SELECT
    s.offset,
    CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN 0 ELSE 1 END AS kind,
    CASE
        WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.sender
        WHEN s.safe_input_index IS NOT NULL THEN d.sender
        ELSE NULL
    END AS sender,
    CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.data ELSE NULL END AS data,
    CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN f.fee ELSE NULL END AS fee,
    CASE WHEN s.safe_input_index IS NOT NULL THEN d.payload ELSE NULL END AS payload,
    CASE WHEN s.safe_input_index IS NOT NULL THEN d.block_number ELSE NULL END AS block_number
FROM sequenced_l2_txs s
LEFT JOIN user_ops u
  ON u.batch_index = s.batch_index
 AND u.frame_in_batch = s.frame_in_batch
 AND u.pos_in_frame = s.user_op_pos_in_frame
LEFT JOIN frames f
  ON f.batch_index = s.batch_index
 AND f.frame_in_batch = s.frame_in_batch
LEFT JOIN safe_inputs d
  ON d.safe_input_index = s.safe_input_index
WHERE s.offset > ?1
  AND s.batch_index NOT IN (SELECT batch_index FROM invalid_batches)
ORDER BY s.offset ASC
LIMIT ?2
