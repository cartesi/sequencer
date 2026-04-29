-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local compare = require("watchdog.compare")

local sequencer = {}

function sequencer.new(http, json, base_url)
    assert(type(http) == "table" and type(http.get) == "function", "http.get is required")
    assert(type(json) == "table" and type(json.decode) == "function", "json.decode is required")
    assert(type(base_url) == "string" and base_url ~= "", "base_url is required")

    local client = {
        http = http,
        json = json,
        base_url = base_url:gsub("/+$", ""),
    }

    function client:get_state()
        local response, err = self.http:get(self.base_url .. "/get_state")
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
            return nil, "invalid get_state response JSON"
        end
        if type(decoded.safe_block) ~= "number" then
            return nil, "safe_block must be a number"
        end
        local ok, validation_err = compare.assert_state_response(decoded.state)
        if not ok then
            return nil, validation_err
        end
        return decoded
    end

    return client
end

return sequencer
