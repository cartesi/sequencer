SELECT
    b.batch_index,
    b.created_at_ms,
    (
        SELECT COUNT(*)
        FROM user_ops u
        WHERE u.batch_index = b.batch_index
    ) AS user_op_count
FROM batches b
ORDER BY b.batch_index DESC
LIMIT 1
