INSERT INTO user_ops (
    batch_index,
    frame_in_batch,
    pos_in_frame,
    sender,
    nonce,
    max_fee,
    data,
    sig,
    received_at_ms
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
