-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local jsonrpc = {}

local function quantity(value)
    assert(type(value) == "number" and value >= 0, "quantity must be a non-negative number")
    return string.format("0x%x", value)
end

local function strip_0x(value)
    return tostring(value):gsub("^0[xX]", "")
end

local function topic_address(address)
    assert(type(address) == "string" and address ~= "", "topic address is required")
    local raw = strip_0x(address):lower()
    assert(#raw == 40 and raw:match("^[0-9a-f]+$") ~= nil, "topic address must be 20-byte hex")
    return "0x" .. string.rep("0", 24) .. raw
end

function jsonrpc.new(http, json, url)
    assert(type(http) == "table" and type(http.post) == "function", "http.post is required")
    assert(type(json) == "table" and type(json.encode) == "function", "json.encode is required")
    assert(type(json.decode) == "function", "json.decode is required")
    assert(type(url) == "string" and url ~= "", "url is required")

    local client = {
        http = http,
        json = json,
        url = url,
        next_id = 1,
    }

    function client:call(method, params)
        local id = self.next_id
        self.next_id = self.next_id + 1
        local body = self.json.encode({
            jsonrpc = "2.0",
            id = id,
            method = method,
            params = params or {},
        })

        local response, http_err = self.http:post(self.url, body, {
            ["content-type"] = "application/json",
        })
        if not response then
            return nil, http_err
        end
        if response.status < 200 or response.status >= 300 then
            return nil, "HTTP " .. tostring(response.status)
        end

        local decoded
        local ok = pcall(function()
            decoded = self.json.decode(response.body)
        end)
        if not ok then
            return nil, "invalid JSON-RPC response JSON"
        end
        if type(decoded) ~= "table" then
            return nil, "JSON-RPC response must be an object"
        end
        if decoded.id ~= id then
            return nil, "JSON-RPC response id mismatch"
        end
        if decoded.error ~= nil then
            local code = decoded.error.code or "unknown"
            local message = decoded.error.message or "JSON-RPC error"
            return nil, tostring(code) .. ": " .. tostring(message)
        end
        return decoded.result
    end

    function client:get_logs(filter)
        assert(type(filter.input_added_topic) == "string", "input_added_topic is required")
        local topics = { filter.input_added_topic }
        if filter.app_address then
            topics[2] = topic_address(filter.app_address)
        end

        return self:call("eth_getLogs", {
            {
                address = filter.address,
                fromBlock = quantity(filter.from_block),
                toBlock = quantity(filter.to_block),
                topics = topics,
            },
        })
    end

    function client:get_block_number_by_tag(tag)
        local block, err = self:call("eth_getBlockByNumber", { tag, false })
        if not block then
            return nil, err
        end
        if type(block.number) ~= "string" then
            return nil, "block response missing number"
        end
        return tonumber(block.number:gsub("^0[xX]", ""), 16)
    end

    return client
end

return jsonrpc
