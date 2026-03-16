SELECT safe_input_index, sender, payload, block_number
FROM safe_inputs
WHERE safe_input_index >= ?1 AND safe_input_index < ?2
ORDER BY safe_input_index ASC
