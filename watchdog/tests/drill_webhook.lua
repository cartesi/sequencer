-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)
--
-- Staging/local drill: fire sample watchdog alarm payloads at WATCHDOG_WEBHOOK_URL.
-- Usage:
--   WATCHDOG_WEBHOOK_URL=https://example.com/hook lua watchdog/tests/drill_webhook.lua

package.path = "./?.lua;./?/init.lua;" .. package.path

local deps_lua = os.getenv("WATCHDOG_LUA_DEPS")
if deps_lua and deps_lua ~= "" then
    package.cpath = deps_lua .. "/?.so;" .. package.cpath
end

local alarm = require("watchdog.alarm")
local http_mod = require("watchdog.http")
local log = dofile("watchdog/tests/e2e_log.lua")

local webhook_url = os.getenv("WATCHDOG_WEBHOOK_URL")
if not webhook_url or webhook_url == "" then
    io.stderr:write("WATCHDOG_WEBHOOK_URL is required\n")
    os.exit(1)
end

local drills = {
    {
        name = "state_mismatch",
        payload = {
            kind = "state_mismatch",
            previous_safe_block = 10,
            sequencer_inclusion_block = 12,
            mismatch_offset = 4,
            run_id = "drill-state-mismatch",
        },
    },
    {
        name = "inclusion_block_regressed",
        payload = {
            kind = "inclusion_block_regressed",
            previous_safe_block = 20,
            sequencer_inclusion_block = 19,
            run_id = "drill-inclusion-block-regressed",
        },
    },
}

local http = http_mod.new()
local failures = 0

log.banner("webhook-delivery-drills")
for index, drill in ipairs(drills) do
    log.step("webhook-delivery-drills", index, #drills, "POST " .. drill.name .. " payload")
    local ok, err = alarm.send_webhook(http, webhook_url, drill.payload)
    if not ok then
        failures = failures + 1
        log.fail(drill.name, err)
    else
        log.pass(drill.name, "HTTP 2xx from " .. webhook_url)
    end
end

if failures > 0 then
    os.exit(1)
end
