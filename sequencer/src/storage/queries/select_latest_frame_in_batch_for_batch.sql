SELECT
    f.frame_in_batch,
    f.fee
FROM frames f
WHERE f.batch_index = ?1
ORDER BY f.frame_in_batch DESC
LIMIT 1
