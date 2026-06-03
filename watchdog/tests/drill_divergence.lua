-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)
--
-- Staging drill: force a state_mismatch alarm through compare mode with a
-- deliberately wrong sequencer state (fake HTTP client). No CM required.
--
-- Usage:
--   WATCHDOG_WEBHOOK_URL=https://example.com/hook lua watchdog/tests/drill_divergence.lua

package.path = "./?.lua;./?/init.lua;" .. package.path

local deps_lua = os.getenv("WATCHDOG_LUA_DEPS")
if deps_lua and deps_lua ~= "" then
    package.cpath = deps_lua .. "/?.so;" .. package.cpath
end

local alarm = require("watchdog.alarm")
local compare = require("watchdog.compare")
local http_mod = require("watchdog.http")
local log = dofile("watchdog/tests/e2e_log.lua")

local webhook_url = os.getenv("WATCHDOG_WEBHOOK_URL")
if not webhook_url or webhook_url == "" then
    io.stderr:write("WATCHDOG_WEBHOOK_URL is required\n")
    os.exit(1)
end

local sequencer_state = '{"balances":{"0x01":1},"nonces":{}}'
local cm_state = '{"balances":{},"nonces":{}}'
local equal, mismatch_offset = compare.raw_equal(sequencer_state, cm_state)
if equal then
    log.fail("divergence-setup", "expected mismatch for drill")
    os.exit(1)
end

local payload = {
    kind = "state_mismatch",
    previous_safe_block = 0,
    sequencer_safe_block = 0,
    mismatch_offset = mismatch_offset,
    run_id = "drill-forced-divergence",
}

log.banner("divergence-webhook-drill")
log.step("divergence-webhook-drill", 1, 1, "POST state_mismatch payload (synthetic bytes)")

local http = http_mod.new()
local ok, err = alarm.send_webhook(http, webhook_url, payload)
if not ok then
    log.fail("divergence-webhook-drill", err)
    os.exit(1)
end

log.pass(
    "divergence-webhook-drill",
    string.format("HTTP 2xx; mismatch_offset=%s", tostring(mismatch_offset))
)
os.exit(0)
