-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Minimal Ethereum JSON-RPC client. Calls return `result` or `nil, message`:
--- the L1 reader inspects provider error codes to split long log ranges.

local jsonrpc = {}

local function quantity(value)
    assert(math.type(value) == "integer" and value >= 0, "quantity must be a non-negative integer")
    return string.format("0x%x", value)
end

local function hex_to_integer(value)
    if type(value) ~= "string" or value:match("^0[xX]%x+$") == nil then
        return nil
    end
    return math.tointeger(tonumber(value:sub(3), 16))
end
jsonrpc.hex_to_integer = hex_to_integer

function jsonrpc.new(http, json, url)
    assert(type(url) == "string" and url ~= "", "RPC url is required")

    local client = { next_id = 1 }

    function client:call(method, params)
        local id = self.next_id
        self.next_id = id + 1
        local response, http_err = http:post(url, json.encode({
            jsonrpc = "2.0",
            id = id,
            method = method,
            params = params,
        }), { ["content-type"] = "application/json" })
        if not response then
            return nil, http_err
        end
        local ok, decoded = pcall(json.decode, response.body)
        if response.status < 200 or response.status >= 300 then
            -- Providers also send JSON-RPC errors, range limits included, with
            -- an HTTP error status; keep them visible, as alloy does.
            local rpc_error = ok and type(decoded) == "table" and decoded.error
            if type(rpc_error) == "table" then
                return nil, string.format("%s: HTTP %d: %s: %s", method, response.status,
                    tostring(rpc_error.code), tostring(rpc_error.message))
            end
            return nil, string.format("%s: HTTP %d: %s", method, response.status, response.body:sub(1, 200))
        end
        if not ok or type(decoded) ~= "table" or decoded.id ~= id then
            return nil, method .. ": malformed JSON-RPC response"
        end
        if decoded.error ~= nil then
            return nil, string.format("%s: %s: %s", method, tostring(decoded.error.code),
                tostring(decoded.error.message))
        end
        return decoded.result
    end

    function client:get_logs(filter)
        return self:call("eth_getLogs", { {
            address = filter.address,
            fromBlock = quantity(filter.from_block),
            toBlock = quantity(filter.to_block),
            topics = filter.topics,
        } })
    end

    function client:chain_id()
        local result, err = self:call("eth_chainId", {})
        if not result then
            return nil, err
        end
        local value = hex_to_integer(result)
        if not value then
            return nil, "eth_chainId: invalid result " .. tostring(result)
        end
        return value
    end

    --- Number of the block a tag (`safe`, `latest`, ...) names.
    function client:block_number(tag)
        local block, err = self:call("eth_getBlockByNumber", { tag, false })
        if not block then
            return nil, err
        end
        local number = type(block) == "table" and hex_to_integer(block.number)
        if not number then
            return nil, "eth_getBlockByNumber: block without a number"
        end
        return number
    end

    --- The code at `address`, as hex; `block` is an integer or tag.
    function client:get_code(address, block)
        local at = math.type(block) == "integer" and quantity(block) or block
        return self:call("eth_getCode", { address, at })
    end

    --- `eth_call` returning the raw hex result; `block` is an integer or tag.
    function client:eth_call(to, data, block)
        local at = math.type(block) == "integer" and quantity(block) or block
        return self:call("eth_call", { { to = to, data = data }, at })
    end

    return client
end

return jsonrpc
