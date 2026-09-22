-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- The application's InputBox inputs, read from L1 and proven complete.
---
--- `inputs(from, to, first_index, on_input)` yields every input of blocks
--- `from..to` in order, and guarantees there is no gap: indices run
--- contiguously from `first_index`, and the InputBox's own count at `to`
--- (read with `getNumberOfInputs` pinned at that block) equals the next index.
--- A provider that silently truncates a log range therefore fails the tick
--- instead of producing a false comparison. Long ranges are split on the
--- provider error codes that mean "range too large", like the Rust reader.

local abi = require("watchdog.abi")
local errors = require("watchdog.errors")
local jsonrpc = require("watchdog.jsonrpc")

local l1 = {}

l1.INPUT_ADDED_TOPIC = "0xc05d337121a6e8605c6ec0b72aa29c4210ffe6e5b9cefdd6a7058188a8f66f98"

-- Bare codes, matching the sequencer's DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES.
-- `-32005` / `-32012` are provider-overloaded (rate-limit vs range); both sides
-- accept that.
l1.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES = { "-32005", "-32012", "-32600", "-32602", "-32616" }

local GET_NUMBER_OF_INPUTS = "0x61a93c87" -- InputBox.getNumberOfInputs(address)
local GET_TEMPLATE_HASH = "0x61b12c66" -- Application.getTemplateHash()

local function address_word(address)
    return string.rep("0", 24) .. address:sub(3)
end

local function log_position(log, field)
    local value = jsonrpc.hex_to_integer(log[field])
    if not value then
        errors.transient("InputAdded log without a valid %s", field)
    end
    return value
end

local function sort_logs(logs)
    local keys = {}
    for _, log in ipairs(logs) do
        keys[log] = {
            log_position(log, "blockNumber"),
            log_position(log, "transactionIndex"),
            log_position(log, "logIndex"),
        }
    end
    table.sort(logs, function(a, b)
        local ka, kb = keys[a], keys[b]
        if ka[1] ~= kb[1] then
            return ka[1] < kb[1]
        end
        if ka[2] ~= kb[2] then
            return ka[2] < kb[2]
        end
        return ka[3] < kb[3]
    end)
end

local function mentions_any(message, codes)
    for _, code in ipairs(codes) do
        if tostring(message):find(code, 1, true) then
            return true
        end
    end
    return false
end

--- `params`: input_box_address, app_address, chain_id, and optionally
--- long_block_range_error_codes.
function l1.new(rpc, params)
    local codes = params.long_block_range_error_codes or l1.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES
    local topics = { l1.INPUT_ADDED_TOPIC, "0x" .. address_word(params.app_address) }
    local reader = {}

    local function rpc_value(value, err)
        if value == nil then
            errors.transient("L1 RPC: %s", tostring(err))
        end
        return value
    end

    function reader:chain_id()
        return rpc_value(rpc:chain_id())
    end

    --- Fail unless the RPC's safe head has reached `block`.
    function reader:require_safe_head(block)
        local head = rpc_value(rpc:block_number("safe"))
        if head < block then
            errors.transient("L1 RPC safe head %d is behind target block %d", head, block)
        end
    end

    --- InputBox inputs of the application through `block`. Before the
    --- InputBox is deployed there are none; an address that never holds a
    --- contract is a misconfiguration, not an empty history.
    function reader:input_count_at(block)
        local result = rpc_value(rpc:eth_call(params.input_box_address,
            GET_NUMBER_OF_INPUTS .. address_word(params.app_address), block))
        if result == "0x" then
            if rpc_value(rpc:get_code(params.input_box_address, "latest")) == "0x" then
                errors.operator("no contract at the InputBox address %s", params.input_box_address)
            end
            return 0
        end
        local ok, count = pcall(abi.decode_uint, result)
        if not ok then
            errors.transient("getNumberOfInputs returned %s", tostring(result))
        end
        return count
    end

    --- The template hash the application was deployed with, as raw bytes.
    function reader:template_hash()
        local result = rpc_value(rpc:eth_call(params.app_address, GET_TEMPLATE_HASH, "latest"))
        if type(result) ~= "string" or #result ~= 66 then
            errors.transient("getTemplateHash returned %s", tostring(result))
        end
        return abi.bytes_from_hex(result)
    end

    local function check(input, log, expected_index)
        if input.block_number ~= log_position(log, "blockNumber") then
            errors.transient("InputAdded log block %s disagrees with its payload block %d",
                tostring(log.blockNumber), input.block_number)
        end
        if input.index ~= expected_index then
            errors.transient("InputBox index %d where %d was expected: the L1 view is incomplete",
                input.index, expected_index)
        end
        if input.app_contract ~= params.app_address then
            errors.transient("input for application %s where %s was expected",
                input.app_contract, params.app_address)
        end
        if input.chain_id ~= params.chain_id then
            errors.operator("input for chain %d where chain %d is configured", input.chain_id, params.chain_id)
        end
    end

    --- Yield each input of blocks `from..to` to `on_input(input)`, which returns
    --- false to stop early. Returns the next input index and whether the scan
    --- stopped early; a complete scan is checked against the InputBox count.
    function reader:inputs(from, to, first_index, on_input)
        local next_index = first_index
        local stopped = false

        local function scan(lo, hi)
            local logs, err = rpc:get_logs({
                address = params.input_box_address,
                from_block = lo,
                to_block = hi,
                topics = topics,
            })
            if logs == nil then
                if lo < hi and mentions_any(err, codes) then
                    local mid = lo + (hi - lo) // 2
                    scan(lo, mid)
                    if not stopped then
                        scan(mid + 1, hi)
                    end
                    return
                end
                errors.transient("L1 RPC: %s", tostring(err))
            end
            sort_logs(logs)
            for _, log in ipairs(logs) do
                local ok, input = pcall(abi.decode_input_added_log, log)
                if not ok then
                    errors.transient("malformed InputAdded log: %s", tostring(input))
                end
                check(input, log, next_index)
                next_index = next_index + 1
                if on_input(input) == false then
                    stopped = true
                    return
                end
            end
        end

        if from <= to then
            scan(from, to)
        end
        if not stopped then
            local count = self:input_count_at(to)
            if count ~= next_index then
                errors.transient("InputBox counts %d inputs at block %d but the scan reached index %d",
                    count, to, next_index)
            end
        end
        return next_index, stopped
    end

    return reader
end

return l1
