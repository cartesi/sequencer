-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)
--
-- Staging drill: drive watchdog/main.lua divergence handling with injected
-- fake deps (same pattern as watchdog/tests/run.lua).
--
-- Usage:
--   lua watchdog/tests/drill_divergence.lua

package.path = "./?.lua;./?/init.lua;" .. package.path

local deps_lua = os.getenv("WATCHDOG_LUA_DEPS")
if deps_lua and deps_lua ~= "" then
    package.cpath = deps_lua .. "/?.so;" .. package.cpath
end

local main_mod = require("watchdog.main")
local log = dofile("watchdog/tests/e2e_log.lua")

local function fake_cfg()
    return {
        mode = "compare",
        checkpoint_dir = "/tmp/watchdog-drill",
        cm_snapshot_dir = "/tmp/genesis-snapshot",
        cm_snapshot_safe_block = 0,
        input_box_address = "0xinputbox",
        app_address = "0x1111111111111111111111111111111111111111",
        input_added_topic = "0xtopic",
        long_block_range_error_codes = require("watchdog.l1_reader").DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES,
        retry_attempts = 1,
        retry_delay_sec = 0,
    }
end

local function fake_machine(inspect_state)
    return {
        load = function(_self, path, reference_block)
            return { path = path, reference_block = reference_block or 0 }
        end,
        advance = function(_self, instance, _inputs, range)
            instance.reference_block = range.to_block
            return true
        end,
        inspect = function()
            return inspect_state
        end,
        dump = function(_self, instance, snapshot_dir, reference_block)
            instance.reference_block = reference_block
            return true
        end,
    }
end

local captured_event = nil
local deps = {
    checkpoint = {
        load = function(_dir)
            return { snapshot_dir = "/tmp/snapshot", safe_block = 0 }
        end,
        write = function()
            return true
        end,
    },
    sequencer = {
        get_finalized_inclusion_block = function()
            return { inclusion_block = 1, l2_tx_index = 0 }
        end,
        get_finalized_state = function()
            return {
                inclusion_block = 1,
                l2_tx_index = 0,
                state = string.char(0x01, 0x02, 0x03, 0x04),
            }
        end,
    },
    fetch_inputs = function(from_block, to_block)
        if from_block ~= 1 or to_block ~= 1 then
            error(string.format("unexpected input range %s..%s", from_block, to_block))
        end
        return {}
    end,
    machine = fake_machine(string.char(0xAA, 0xBB, 0xCC, 0xDD)),
    on_watchdog_event = function(payload)
        captured_event = payload
    end,
}

log.banner("divergence-signal-drill")
log.step("divergence-signal-drill", 1, 1, "run main.lua compare cycle with injected mismatch deps")

local exit_code, err = main_mod.run_compare_cycle(fake_cfg(), deps)
if exit_code ~= main_mod.EXIT_DIVERGENCE then
    log.fail("divergence-signal-drill", string.format("expected exit %d, got %s (%s)", main_mod.EXIT_DIVERGENCE, tostring(exit_code), tostring(err)))
    os.exit(1)
end
if type(captured_event) ~= "table" or captured_event.kind ~= "state_mismatch" then
    log.fail("divergence-signal-drill", "expected watchdog_event state_mismatch payload")
    os.exit(1)
end
if type(captured_event.mismatch_offset) ~= "number" or captured_event.mismatch_offset < 0 then
    log.fail("divergence-signal-drill", "expected non-negative mismatch_offset in event")
    os.exit(1)
end

log.pass(
    "divergence-signal-drill",
    string.format("main.lua emitted state_mismatch; mismatch_offset=%s", tostring(captured_event.mismatch_offset))
)
os.exit(main_mod.EXIT_DIVERGENCE)
