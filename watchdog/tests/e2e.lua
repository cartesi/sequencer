-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)
--
-- Real watchdog end-to-end checks against cartesi-machine (and optionally a
-- live sequencer). Run from repo root:
--   lua watchdog/tests/e2e.lua
-- or:
--   just test-watchdog-e2e

package.path = "./?.lua;./?/init.lua;" .. package.path

local checkpoint = require("watchdog.checkpoint")
local machine_cli = require("watchdog.machine_cli")
local runner = require("watchdog.runner")
local log = dofile("watchdog/tests/e2e_log.lua")

local MACHINE_IMAGE = "examples/canonical-app/out/canonical-machine-image"
local GENESIS_SAFE_BLOCK = 0

local scenarios = {}
local failures = 0
local skips = 0

local function assert_true(value, message)
    if not value then
        error(message or "assertion failed", 2)
    end
end

local function command_exists(name)
    local ok = os.execute("command -v " .. name .. " >/dev/null 2>&1")
    return ok == true or ok == 0
end

local function path_is_dir(path)
    local ok, err, code = os.rename(path, path)
    if ok then
        return true
    end
    if code == 13 then
        return true
    end
    return false, err
end

local function temp_dir(prefix)
    local base = os.tmpname()
    os.remove(base)
    local dir = string.format("%s-%s", prefix, base:match("([^/]+)$"))
    local ok = os.execute('mkdir -p "' .. dir .. '"')
    if ok ~= true and ok ~= 0 then
        error("mkdir failed for " .. dir)
    end
    return dir
end

local function make_step_logger(scenario, total)
    local index = 0
    return function(message)
        index = index + 1
        log.step(scenario, index, total, message)
    end
end

local function run_scenario(name, fn)
    log.banner(name)
    local ok, result = pcall(fn)
    if not ok then
        failures = failures + 1
        log.fail(name, result)
        return
    end
    if result == "skip" then
        skips = skips + 1
        return
    end
    log.pass(name)
end

local function skip(scenario, reason)
    log.skip(scenario, reason)
    return "skip"
end

local function require_cartesi_machine(scenario)
    if not command_exists("cartesi-machine") then
        return skip(scenario, "cartesi-machine not on PATH (install via nix develop / Cartesi tools)")
    end
    return true
end

local function require_machine_image(scenario)
    if not path_is_dir(MACHINE_IMAGE) then
        return skip(
            scenario,
            "canonical machine image missing at "
                .. MACHINE_IMAGE
                .. " (run: just canonical-build-machine-image)"
        )
    end
    return true
end

local function advance_cfg(checkpoint_dir, target_safe_block)
    return {
        mode = "advance",
        checkpoint_dir = checkpoint_dir,
        cm_snapshot_dir = MACHINE_IMAGE,
        cm_snapshot_safe_block = GENESIS_SAFE_BLOCK,
        target_safe_block = target_safe_block,
        cm_executable = "cartesi-machine",
        cm_work_dir = temp_dir("watchdog-e2e-work"),
    }
end

table.insert(scenarios, {
    name = "prerequisites",
    fn = function()
        local scenario = "prerequisites"
        log.step(scenario, 1, 3, "check cartesi-machine is on PATH")
        if not command_exists("cartesi-machine") then
            error("cartesi-machine not on PATH")
        end
        log.step(scenario, 2, 3, "check canonical machine image directory exists")
        if not path_is_dir(MACHINE_IMAGE) then
            error("missing machine image at " .. MACHINE_IMAGE)
        end
        log.step(scenario, 3, 3, "record paths used by later scenarios")
        log.info("machine image: " .. MACHINE_IMAGE)
        log.info("genesis safe_block: " .. tostring(GENESIS_SAFE_BLOCK))
    end,
})

table.insert(scenarios, {
    name = "advance-empty-range",
    fn = function()
        local scenario = "advance-empty-range"
        if require_cartesi_machine(scenario) == "skip" then
            return "skip"
        end
        if require_machine_image(scenario) == "skip" then
            return "skip"
        end

        local checkpoint_dir = temp_dir("watchdog-e2e-checkpoint")
        local target_safe_block = 1
        log.step(scenario, 1, 5, "prepare temp checkpoint dir: " .. checkpoint_dir)
        log.step(scenario, 2, 5, "bootstrap from genesis snapshot at safe_block=" .. GENESIS_SAFE_BLOCK)
        log.step(scenario, 3, 5, "run advance mode with empty L1 input range 1.." .. target_safe_block)
        local result, err = runner.advance_checkpoint_once(advance_cfg(checkpoint_dir, target_safe_block), {
            machine = machine_cli.new({
                executable = "cartesi-machine",
                work_dir = temp_dir("watchdog-e2e-advance-work"),
            }),
            log_step = make_step_logger(scenario .. "/runner", 8),
            fetch_inputs = function(from_block, to_block)
                assert_true(from_block == 1, "expected from_block=1")
                assert_true(to_block == target_safe_block, "expected to_block=" .. target_safe_block)
                return {}
            end,
        })
        assert_true(result, "advance failed: " .. tostring(err))
        assert_true(result.safe_block == target_safe_block, "unexpected safe_block")
        assert_true(result.input_count == 0, "expected zero inputs")

        log.step(scenario, 4, 5, "verify manifest-backed checkpoint was written")
        local current = checkpoint.load(checkpoint_dir)
        assert_true(current, "checkpoint current.json missing after advance")
        assert_true(current.safe_block == target_safe_block, "checkpoint safe_block mismatch")

        log.step(scenario, 5, 5, "verify snapshot directory exists under checkpoint")
        assert_true(path_is_dir(current.snapshot_dir), "checkpoint snapshot dir missing")
        log.info("wrote checkpoint safe_block=" .. tostring(current.safe_block))
    end,
})

table.insert(scenarios, {
    name = "cm-inspect-state-query",
    fn = function()
        local scenario = "cm-inspect-state-query"
        if require_cartesi_machine(scenario) == "skip" then
            return "skip"
        end
        if require_machine_image(scenario) == "skip" then
            return "skip"
        end

        log.step(scenario, 1, 4, "create machine_cli adapter")
        local work_dir = temp_dir("watchdog-e2e-inspect")
        local machine = machine_cli.new({
            executable = "cartesi-machine",
            work_dir = work_dir,
        })

        log.step(scenario, 2, 4, "load genesis snapshot from " .. MACHINE_IMAGE)
        local instance = assert(machine:load(MACHINE_IMAGE), "load snapshot failed")

        log.step(scenario, 3, 4, "run --cmio-inspect-state with query=state (no new inputs)")
        local report, inspect_err = machine:inspect_state(instance)
        assert_true(report, "inspect failed: " .. tostring(inspect_err))

        log.step(scenario, 4, 4, "validate inspect report is SSZ (not legacy JSON)")
        if report:find("inspect endpoint not implemented", 1, true) then
            return skip(
                scenario,
                "machine image dapp is stale; rebuild with: just canonical-build-machine-image"
            )
        end
        if report:sub(1, 1) == "{" then
            return skip(
                scenario,
                "machine image still returns JSON export_state; rebuild with: just canonical-build-machine-image"
            )
        end
        assert_true(#report >= 76, "inspect SSZ report too short: " .. tostring(#report))
        log.info("inspect report bytes=" .. tostring(#report))
    end,
})

table.insert(scenarios, {
    name = "compare-runner-with-sequencer",
    fn = function()
        local scenario = "compare-runner-with-sequencer"
        local sequencer_url = os.getenv("WATCHDOG_E2E_SEQUENCER_URL")
        if not sequencer_url or sequencer_url == "" then
            return skip(
                scenario,
                "set WATCHDOG_E2E_SEQUENCER_URL to a live sequencer base URL to run this scenario"
            )
        end
        if require_cartesi_machine(scenario) == "skip" then
            return "skip"
        end
        if require_machine_image(scenario) == "skip" then
            return "skip"
        end

        local http_mod = require("watchdog.http")
        local jsonrpc = require("watchdog.jsonrpc")
        local sequencer_reader = require("watchdog.sequencer_reader")
        local json = require("watchdog.json").new()

        local checkpoint_dir = temp_dir("watchdog-e2e-compare")
        log.step(scenario, 1, 2, "prepare compare-mode deps (sequencer=" .. sequencer_url .. ")")
        log.step(scenario, 2, 2, "run compare runner against live sequencer + CM")

        local http = http_mod.new()
        local cfg = {
            mode = "compare",
            sequencer_url = sequencer_url,
            checkpoint_dir = checkpoint_dir,
            cm_snapshot_dir = MACHINE_IMAGE,
            cm_snapshot_safe_block = GENESIS_SAFE_BLOCK,
            cm_executable = "cartesi-machine",
            cm_work_dir = temp_dir("watchdog-e2e-compare-work"),
            l1_rpc_url = os.getenv("WATCHDOG_E2E_L1_RPC_URL") or "http://127.0.0.1:8545",
            input_box_address = os.getenv("WATCHDOG_E2E_INPUTBOX_ADDRESS")
                or "0x0000000000000000000000000000000000000000",
            app_address = os.getenv("WATCHDOG_E2E_APP_ADDRESS")
                or "0x1111111111111111111111111111111111111111",
            long_block_range_error_codes = { "-32005" },
        }

        local step_no = 0
        local result, err = runner.run_once(cfg, {
            http = http,
            rpc = jsonrpc.new(http, json, cfg.l1_rpc_url),
            sequencer = sequencer_reader.new(http, json, sequencer_url),
            machine = machine_cli.new({
                executable = cfg.cm_executable,
                work_dir = cfg.cm_work_dir,
            }),
            log_step = function(message)
                step_no = step_no + 1
                log.step(scenario .. "/runner", step_no, 12, message)
            end,
        })

        assert_true(result, "compare run failed: " .. tostring(err))
        log.info(string.format(
            "compare ok: safe_block=%s input_count=%s",
            tostring(result.safe_block),
            tostring(result.input_count)
        ))
    end,
})

log.info("starting watchdog real end-to-end suite (" .. #scenarios .. " scenarios)")
for _, scenario in ipairs(scenarios) do
    run_scenario(scenario.name, scenario.fn)
end

io.write("\n[watchdog-e2e] ───────────────────────────────────────────────────────\n")
io.write(string.format(
    "[watchdog-e2e] SUMMARY: %d passed, %d skipped, %d failed (of %d scenarios)\n",
    #scenarios - failures - skips,
    skips,
    failures,
    #scenarios
))

if failures > 0 then
    os.exit(1)
end
