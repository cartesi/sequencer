CREATE VIEW IF NOT EXISTS frame_drain_ranges AS
SELECT
    batch_index,
    frame_in_batch,
    COALESCE(
        SUM(drain_n) OVER (
            ORDER BY batch_index, frame_in_batch
            ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
        ),
        0
    ) AS direct_start_index,
    drain_n
FROM frame_drains;

CREATE VIEW IF NOT EXISTS ordered_sequenced_l2_txs AS
SELECT
    u.batch_index AS batch_index,
    u.frame_in_batch AS frame_in_batch,
    0 AS kind,
    u.pos_in_frame AS pos,
    u.sender AS sender,
    u.data AS data,
    b.fee AS fee,
    NULL AS payload
FROM user_ops u
JOIN batches b
  ON b.batch_index = u.batch_index

UNION ALL

SELECT
    d.batch_index AS batch_index,
    d.frame_in_batch AS frame_in_batch,
    1 AS kind,
    (i.direct_input_index - d.direct_start_index) AS pos,
    NULL AS sender,
    NULL AS data,
    NULL AS fee,
    i.payload AS payload
FROM frame_drain_ranges d
JOIN direct_inputs i
  ON i.direct_input_index >= d.direct_start_index
 AND i.direct_input_index < (d.direct_start_index + d.drain_n);

CREATE VIEW IF NOT EXISTS batch_user_op_counts AS
SELECT
    b.batch_index AS batch_index,
    b.created_at_ms AS created_at_ms,
    b.fee AS fee,
    COALESCE(c.user_op_count, 0) AS user_op_count
FROM batches b
LEFT JOIN (
    SELECT
        u.batch_index AS batch_index,
        COUNT(*) AS user_op_count
    FROM user_ops u
    GROUP BY u.batch_index
) c ON c.batch_index = b.batch_index;

CREATE VIEW IF NOT EXISTS frame_user_op_counts AS
SELECT
    u.batch_index AS batch_index,
    u.frame_in_batch AS frame_in_batch,
    COUNT(*) AS user_op_count
FROM user_ops u
GROUP BY u.batch_index, u.frame_in_batch;
