SELECT
    batch_index,
    created_at_ms,
    fee,
    user_op_count
FROM batch_user_op_counts
ORDER BY batch_index DESC
LIMIT 1
