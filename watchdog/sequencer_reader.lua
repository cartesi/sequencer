-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local compare = require("watchdog.compare")

local sequencer_reader = {}

local function parse_header_number(headers, name)
    if type(headers) ~= "table" then
        return nil, name .. " header missing"
    end
    local value = headers[name:lower()]
    if value == nil then
        return nil, name .. " header missing"
    end
    local number = tonumber(value)
    if number == nil then
        return nil, name .. " header must be numeric"
    end
    return number
end

function sequencer_reader.new(http, json, base_url)
    assert(type(http) == "table" and type(http.get) == "function", "http.get is required")
    assert(type(json) == "table" and type(json.decode) == "function", "json.decode is required")
    assert(type(base_url) == "string" and base_url ~= "", "base_url is required")

    local client = {
        http = http,
        json = json,
        base_url = base_url:gsub("/+$", ""),
    }

    function client:get_finalized_inclusion_block()
        local response, err = self.http:get(self.base_url .. "/finalized_state/inclusion_block")
        if not response then
            return nil, err
        end
        if response.status < 200 or response.status >= 300 then
            return nil, "HTTP " .. tostring(response.status)
        end

        local decoded
        local ok_decode = pcall(function()
            decoded = self.json.decode(response.body)
        end)
        if not ok_decode or type(decoded) ~= "table" then
            return nil, "invalid finalized inclusion_block response JSON"
        end
        if type(decoded.inclusion_block) ~= "number" then
            return nil, "inclusion_block must be a number"
        end
        if type(decoded.l2_tx_index) ~= "number" then
            return nil, "l2_tx_index must be a number"
        end
        return decoded
    end

    function client:get_finalized_state(request_headers)
        local response, err = self.http:get(
            self.base_url .. "/finalized_state",
            request_headers or {}
        )
        if not response then
            return nil, err
        end
        if response.status == 304 then
            return { not_modified = true }
        end
        if response.status < 200 or response.status >= 300 then
            return nil, "HTTP " .. tostring(response.status)
        end

        local inclusion_block, inclusion_err =
            parse_header_number(response.headers, "X-Inclusion-Block")
        if not inclusion_block then
            return nil, inclusion_err
        end
        local l2_tx_index, index_err = parse_header_number(response.headers, "X-L2-Tx-Index")
        if not l2_tx_index then
            return nil, index_err
        end

        local ok, validation_err = compare.assert_state_response(response.body)
        if not ok then
            return nil, validation_err
        end

        return {
            inclusion_block = inclusion_block,
            l2_tx_index = l2_tx_index,
            state = response.body,
            etag = response.headers and response.headers["etag"],
        }
    end

    return client
end

return sequencer_reader
