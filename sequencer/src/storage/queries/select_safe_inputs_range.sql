SELECT direct_input_index, payload
FROM direct_inputs
WHERE direct_input_index >= ?1 AND direct_input_index < ?2
ORDER BY direct_input_index ASC
