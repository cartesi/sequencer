-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local alarm = {}

local function encode_json_object(fields)
    local parts = {}
    for key, value in pairs(fields) do
        local encoded_value
        if type(value) == "number" then
            encoded_value = tostring(value)
        elseif type(value) == "boolean" then
            encoded_value = value and "true" or "false"
        else
            local string_value = tostring(value)
                :gsub("\\", "\\\\")
                :gsub('"', '\\"')
                :gsub("\n", "\\n")
            encoded_value = '"' .. string_value .. '"'
        end
        table.insert(parts, string.format('"%s":%s', key, encoded_value))
    end
    return "{" .. table.concat(parts, ",") .. "}"
end

function alarm.send_webhook(http, webhook_url, payload)
    if not webhook_url or webhook_url == "" then
        return nil, "WATCHDOG_WEBHOOK_URL is not configured"
    end
    local body = encode_json_object(payload)
    local response, err = http:post(webhook_url, body, {
        ["content-type"] = "application/json",
    })
    if not response then
        return nil, err
    end
    if response.status < 200 or response.status >= 300 then
        return nil, "alarm webhook HTTP " .. tostring(response.status)
    end
    return true
end

return alarm
