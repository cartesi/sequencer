SELECT COUNT(*)
FROM user_ops
WHERE batch_index = ?1 AND frame_in_batch = ?2
