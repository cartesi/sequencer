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
local machine_cartesi = require("watchdog.machine_cartesi")
local runner = require("watchdog.runner")
local log = dofile("watchdog/tests/e2e_log.lua")

local MACHINE_IMAGE = "examples/canonical-app/out/canonical-machine-image"
local GENESIS_SAFE_BLOCK = 0

local scenarios = {}
local failures = 0
local skips = 0
local machine_cartesi_probe = nil

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
    -- Keep os.tmpname()'s full path (under the system temp dir) so scratch dirs
    -- land in TMPDIR, not the repo root. Stripping the dir left them in cwd.
    local base = os.tmpname()
    os.remove(base)
    local dir = string.format("%s-%s", base, prefix)
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

-- Toolchain prerequisites are hard requirements, not skips: in CI a missing
-- dep must fail the run, never pass vacuously. (The genuinely-optional
-- live-sequencer scenario still skips on its own, below.)
local function require_cartesi_machine()
    if not command_exists("cartesi-machine") then
        error("cartesi-machine not on PATH (install via nix develop / Cartesi tools)", 0)
    end
end

local function require_machine_image()
    if not path_is_dir(MACHINE_IMAGE) then
        error(
            "canonical machine image missing at " .. MACHINE_IMAGE .. " (run: just canonical-build-machine-image)",
            0
        )
    end
end

-- A missing in-process cartesi binding is the exact failure that must not pass
-- silently in CI (the prerequisites scenario does not cover it). Probe is
-- cached so the machine is only loaded once.
local function require_machine_cartesi_binding()
    if machine_cartesi_probe == nil then
        local machine = machine_cartesi.new()
        local instance, err = machine:load(MACHINE_IMAGE)
        if instance then
            machine_cartesi_probe = { ok = true }
        else
            machine_cartesi_probe = {
                ok = false,
                err = "machine_cartesi binding unavailable on this host: " .. tostring(err),
            }
        end
    end
    if not machine_cartesi_probe.ok then
        error(machine_cartesi_probe.err, 0)
    end
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
    name = "cm-inspect-state-query",
    fn = function()
        local scenario = "cm-inspect-state-query"
        require_cartesi_machine()
        require_machine_image()
        require_machine_cartesi_binding()

        log.step(scenario, 1, 4, "create machine_cartesi adapter")
        local machine = machine_cartesi.new()

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
        require_cartesi_machine()
        require_machine_image()
        require_machine_cartesi_binding()

        local http_mod = require("watchdog.http")
        local jsonrpc = require("watchdog.jsonrpc")
        local sequencer_reader = require("watchdog.sequencer_reader")
        local json = require("watchdog.json").new()
        local main_mod = require("watchdog.main")

        local state_dir = temp_dir("watchdog-e2e-compare")
        log.step(scenario, 1, 2, "prepare watchdog deps (sequencer=" .. sequencer_url .. ")")
        log.step(scenario, 2, 2, "run compare runner against live sequencer + CM")

        local http = http_mod.new()
        local cfg = {
            sequencer_url = sequencer_url,
            state_dir = state_dir,
            cm_snapshot_dir = MACHINE_IMAGE,
            cm_snapshot_safe_block = GENESIS_SAFE_BLOCK,
            l1_rpc_url = os.getenv("WATCHDOG_E2E_L1_RPC_URL") or "http://127.0.0.1:8545",
            input_box_address = os.getenv("WATCHDOG_E2E_INPUTBOX_ADDRESS")
                or "0x0000000000000000000000000000000000000000",
            app_address = os.getenv("WATCHDOG_E2E_APP_ADDRESS")
                or "0x1111111111111111111111111111111111111111",
            long_block_range_error_codes = { "-32005" },
            retry_attempts = 1,
            retry_delay_sec = 0,
        }

        assert_true(main_mod.run_init(cfg, {
            machine = machine_cartesi.new(),
        }), "watchdog init failed")

        local step_no = 0
        local result, err = runner.run_once(cfg, {
            http = http,
            rpc = jsonrpc.new(http, json, cfg.l1_rpc_url),
            sequencer = sequencer_reader.new(http, json, sequencer_url),
            machine = machine_cartesi.new(),
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

table.insert(scenarios, {
    name = "machine-cartesi-store-reload-advance",
    fn = function()
        local scenario = "machine-cartesi-store-reload-advance"
        require_cartesi_machine()
        require_machine_image()
        require_machine_cartesi_binding()

        local checkpoint_dir = temp_dir("watchdog-e2e-store-reload")

        -- Advance the in-process CM one block from `source_dir` and store a
        -- checkpoint at `block` via checkpoint.write (the production store path).
        local function advance_and_store(source_dir, prev_block, block)
            local machine = machine_cartesi.new()
            local instance = assert(machine:load(source_dir, prev_block), "load failed")
            assert(machine:advance(instance, {}, { from_block = prev_block + 1, to_block = block }))
            return checkpoint.write(checkpoint_dir, block, function(snapshot_dir)
                return machine:dump(instance, snapshot_dir, block)
            end, { created_at = "2026-01-01T00:00:00Z" })
        end

        log.step(scenario, 1, 4, "advance genesis -> block 1 and store a checkpoint")
        assert_true(advance_and_store(MACHINE_IMAGE, GENESIS_SAFE_BLOCK, 1), "first store failed")

        log.step(scenario, 2, 4, "reload the stored block-1 snapshot and advance -> block 2")
        local reloaded = checkpoint.load(checkpoint_dir)
        assert_true(reloaded and reloaded.safe_block == 1, "checkpoint at block 1 missing")
        assert_true(advance_and_store(reloaded.snapshot_dir, 1, 2), "reload + store failed")

        log.step(scenario, 3, 4, "load current checkpoint metadata (block 1 should be GC'd)")
        local current = checkpoint.load(checkpoint_dir)
        assert_true(current and current.safe_block == 2, "current safe_block mismatch")

        log.step(scenario, 4, 4, "verify snapshot directory exists")
        assert_true(path_is_dir(current.snapshot_dir), "stored snapshot directory missing")
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
