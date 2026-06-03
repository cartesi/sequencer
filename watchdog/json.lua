-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Watchdog JSON facade (pure Lua; see third_party/json.lua).

local json = {}

function json.new()
    return require("watchdog.third_party.json")
end

return json
