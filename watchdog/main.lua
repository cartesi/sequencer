-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

package.path = "./?.lua;./?/init.lua;" .. package.path

local config = require("watchdog.config")
local http_mod = require("watchdog.http")
local json_mod = require("watchdog.json")
local jsonrpc = require("watchdog.jsonrpc")
local machine_cartesi = require("watchdog.machine_cartesi")
local retry = require("watchdog.retry")
local runner = require("watchdog.runner")
local sequencer_reader = require("watchdog.sequencer_reader")

local EXIT_OK = 0
local EXIT_TRANSIENT = 1
local EXIT_DIVERGENCE = 2

local function prepend_deps_cpath()
    local deps = os.getenv("WATCHDOG_LUA_DEPS")
    if deps and deps ~= "" then
        package.cpath = deps .. "/?.so;" .. package.cpath
    end
end

local function load_machine(_cfg)
    return machine_cartesi.new()
end

local function run_once(cfg, deps)
    if cfg.mode == "compare" then
        return runner.run_once(cfg, deps)
    end
    return runner.advance_checkpoint_once(cfg, deps)
end

local function is_table_with_kind(value)
    return type(value) == "table" and type(value.kind) == "string"
end

local function is_terminal_error(value)
    if not is_table_with_kind(value) then
        return false
    end
    return value.kind == "state_mismatch"
        or value.kind == "inclusion_block_regressed"
        or value.kind == "safe_block_regressed"
end

local function emit_watchdog_event(json, payload)
    io.stderr:write("watchdog_event " .. json.encode(payload) .. "\n")
end

local function main()
    prepend_deps_cpath()
    local cfg = config.load()
    local http = http_mod.new()
    local json = json_mod.new()
    local deps = {
        http = http,
        rpc = jsonrpc.new(http, json, cfg.l1_rpc_url),
        machine = load_machine(cfg),
        log_step = function(message)
            io.stderr:write("watchdog_step " .. tostring(message) .. "\n")
        end,
    }
    if cfg.sequencer_url then
        deps.sequencer = sequencer_reader.new(http, json, cfg.sequencer_url)
    end

    repeat
        local result, err = retry.with_retries(function()
            local ok, value, run_err = pcall(run_once, cfg, deps)
            if not ok then
                return nil, value
            end
            return value, run_err
        end, {
            attempts = cfg.retry_attempts,
            delay_sec = cfg.retry_delay_sec,
            should_retry = function(retry_err)
                return not is_terminal_error(retry_err)
            end,
        })
        if result == nil then
            if is_terminal_error(err) then
                emit_watchdog_event(json, err)
                os.exit(EXIT_DIVERGENCE)
            end
            io.stderr:write("watchdog run failed: " .. tostring(err) .. "\n")
            os.exit(EXIT_TRANSIENT)
        end
        if cfg.once then
            os.exit(EXIT_OK)
        end
        os.execute("sleep " .. tostring(cfg.poll_interval_sec))
    until false
end

if ... == nil then
    main()
end

return {
    main = main,
    run_once = run_once,
}
