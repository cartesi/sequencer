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

local function main()
    prepend_deps_cpath()
    local cfg = config.load()
    local http = http_mod.new()
    local json = json_mod.new()
    local deps = {
        http = http,
        rpc = jsonrpc.new(http, json, cfg.l1_rpc_url),
        machine = load_machine(cfg),
    }
    if cfg.sequencer_url then
        deps.sequencer = sequencer_reader.new(http, json, cfg.sequencer_url)
    end

    repeat
        local result, err = retry.with_retries(function()
            local ok, value = pcall(run_once, cfg, deps)
            if not ok then
                return nil, value
            end
            return value
        end, {
            attempts = cfg.retry_attempts,
            delay_sec = cfg.retry_delay_sec,
        })
        if result == nil then
            io.stderr:write("watchdog run failed: " .. tostring(err) .. "\n")
            os.exit(1)
        end
        if cfg.once then
            return
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
