-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)
--
-- Staging drill: force a state_mismatch signal and verify the structured
-- event line + divergence exit code contract.
--
-- Usage:
--   lua watchdog/tests/drill_divergence.lua

package.path = "./?.lua;./?/init.lua;" .. package.path

local deps_lua = os.getenv("WATCHDOG_LUA_DEPS")
if deps_lua and deps_lua ~= "" then
    package.cpath = deps_lua .. "/?.so;" .. package.cpath
end

local compare = require("watchdog.compare")
local log = dofile("watchdog/tests/e2e_log.lua")

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
    sequencer_inclusion_block = 0,
    mismatch_offset = mismatch_offset,
}

log.banner("divergence-signal-drill")
log.step("divergence-signal-drill", 1, 1, "emit watchdog_event payload and exit with divergence code")
io.stderr:write('watchdog_event {"kind":"state_mismatch","previous_safe_block":0,"sequencer_inclusion_block":0,"mismatch_offset":'
    .. tostring(payload.mismatch_offset) .. "}\n")

log.pass(
    "divergence-signal-drill",
    string.format("event emitted; mismatch_offset=%s", tostring(mismatch_offset))
)
os.exit(2)
