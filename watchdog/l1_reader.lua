-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local abi = require("watchdog.abi")

local l1_reader = {}

l1_reader.INPUT_ADDED_TOPIC = "0xc05d337121a6e8605c6ec0b72aa29c4210ffe6e5b9cefdd6a7058188a8f66f98"

l1_reader.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES = {
    "-32005",
    "-32600",
    "-32602",
    "-32616",
}

local function contains_any(message, codes)
    message = tostring(message or "")
    for _, code in ipairs(codes or {}) do
        if message:find(code, 1, true) then
            return true
        end
    end
    return false
end

local function hex_quantity_to_number(value, field)
    if type(value) == "number" then
        return value
    end
    if type(value) ~= "string" or value:sub(1, 2) ~= "0x" then
        error((field or "quantity") .. " must be an Ethereum hex quantity")
    end
    return tonumber(value:sub(3), 16)
end

local function log_order_key(log)
    return {
        hex_quantity_to_number(log.blockNumber, "blockNumber"),
        hex_quantity_to_number(log.transactionIndex or "0x0", "transactionIndex"),
        hex_quantity_to_number(log.logIndex or "0x0", "logIndex"),
    }
end

function l1_reader.sort_logs(logs)
    table.sort(logs, function(a, b)
        local ak = log_order_key(a)
        local bk = log_order_key(b)
        if ak[1] ~= bk[1] then
            return ak[1] < bk[1]
        end
        if ak[2] ~= bk[2] then
            return ak[2] < bk[2]
        end
        return ak[3] < bk[3]
    end)
    return logs
end

--- Return the RPC latest head when it is at least `target_block`; otherwise a transient retry error.
function l1_reader.ensure_rpc_head_at_least(rpc, target_block)
    assert(type(rpc) == "table" and type(rpc.get_block_number_by_tag) == "function", "rpc.get_block_number_by_tag is required")
    assert(type(target_block) == "number", "target_block is required")

    local head, err = rpc:get_block_number_by_tag("latest")
    if not head then
        return nil, err
    end
    if head < target_block then
        return nil, string.format(
            "L1 RPC latest head %d lags target block %d; retry",
            head,
            target_block
        )
    end
    return head
end

function l1_reader.fetch_logs_partitioned(rpc, params)
    assert(type(rpc) == "table" and type(rpc.get_logs) == "function", "rpc.get_logs is required")
    assert(type(params) == "table", "params are required")

    local start_block = assert(params.start_block, "start_block is required")
    local end_block = assert(params.end_block, "end_block is required")
    local codes = params.long_block_range_error_codes or l1_reader.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES
    local input_added_topic = params.input_added_topic or l1_reader.INPUT_ADDED_TOPIC

    local function go(from_block, to_block)
        local logs, err = rpc:get_logs({
            address = params.input_box_address,
            app_address = params.app_address,
            from_block = from_block,
            to_block = to_block,
            input_added_topic = input_added_topic,
        })
        if logs then
            return logs
        end

        if from_block < to_block and contains_any(err, codes) then
            local mid = from_block + ((to_block - from_block) // 2)
            local left, left_err = go(from_block, mid)
            if not left then
                return nil, left_err
            end
            local right, right_err = go(mid + 1, to_block)
            if not right then
                return nil, right_err
            end
            for _, log in ipairs(right) do
                table.insert(left, log)
            end
            return left
        end

        return nil, err
    end

    if end_block < start_block then
        return {}
    end

    local logs, err = go(start_block, end_block)
    if not logs then
        return nil, err
    end
    return l1_reader.sort_logs(logs)
end

function l1_reader.decode_and_validate_log(log)
    local decoded = abi.decode_input_added_log(log)
    local block_number = hex_quantity_to_number(log.blockNumber, "blockNumber")
    if decoded.block_number ~= block_number then
        error(string.format(
            "InputAdded block number mismatch: log=%d payload=%d",
            block_number,
            decoded.block_number
        ))
    end
    return decoded
end

function l1_reader.fetch_inputs(rpc, params)
    local logs, err = l1_reader.fetch_logs_partitioned(rpc, params)
    if not logs then
        return nil, err
    end

    local inputs = {}
    for _, log in ipairs(logs) do
        table.insert(inputs, l1_reader.decode_and_validate_log(log))
    end
    return inputs
end

return l1_reader
