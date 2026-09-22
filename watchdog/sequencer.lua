-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Client for the sequencer's operator routes for its accepted checkpoint
--- (README.md#operator-snapshot-endpoints-internal-only). Every failure is
--- transient: a missing checkpoint (404) or the sequencer's own divergence
--- refusal (503) clears on the sequencer's side, not the watchdog's.

local errors = require("watchdog.errors")

local sequencer = {}

-- Hashing a multi-GiB comparison file takes seconds on the sequencer.
local DIGEST_TIMEOUT_SEC = 300

function sequencer.new(http, json, base_url)
    base_url = base_url:gsub("/+$", "")
    local client = {}

    local function require_ok(route, response, err)
        if not response then
            errors.transient("sequencer %s: %s", route, tostring(err))
        end
        if response.status < 200 or response.status >= 300 then
            errors.transient("sequencer %s: HTTP %d %s", route, response.status, response.body or "")
        end
        return response
    end

    local function get_json(route, timeout)
        local response = require_ok(route, http:get(base_url .. route, { timeout = timeout }))
        local ok, decoded = pcall(json.decode, response.body)
        if not ok or type(decoded) ~= "table" then
            errors.transient("sequencer %s: malformed JSON", route)
        end
        return decoded
    end

    local function block_of(route, value)
        if math.type(value) ~= "integer" or value < 0 then
            errors.transient("sequencer %s: invalid inclusion_block %s", route, tostring(value))
        end
        return value
    end

    --- The accepted checkpoint's L1 block: a cheap database read.
    function client:inclusion_block()
        local route = "/finalized_state/inclusion_block"
        return block_of(route, get_json(route).inclusion_block)
    end

    --- The accepted checkpoint's block and the SHA-256 of its comparison file.
    function client:digest()
        local route = "/finalized_state/digest"
        local decoded = get_json(route, DIGEST_TIMEOUT_SEC)
        if type(decoded.sha256) ~= "string" or decoded.sha256:match("^%x+$") == nil or #decoded.sha256 ~= 64 then
            errors.transient("sequencer %s: invalid sha256 %s", route, tostring(decoded.sha256))
        end
        return { inclusion_block = block_of(route, decoded.inclusion_block), sha256 = decoded.sha256:lower() }
    end

    --- Download the comparison file into `path`; returns its block.
    function client:download_state(path)
        local route = "/finalized_state"
        local response = require_ok(route, http:download(base_url .. route, path))
        return block_of(route, math.tointeger(tonumber(response.headers["x-inclusion-block"] or "")))
    end

    return client
end

return sequencer
