SELECT
    COALESCE(
        (
            SELECT user_op_count
            FROM frame_user_op_counts
            WHERE batch_index = ?1 AND frame_in_batch = ?2
        ),
        0
    )
