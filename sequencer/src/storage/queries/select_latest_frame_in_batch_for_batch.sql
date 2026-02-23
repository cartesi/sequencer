SELECT f.frame_in_batch
FROM frames f
WHERE f.batch_index = ?1
ORDER BY f.frame_in_batch DESC
LIMIT 1
