-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)
--
-- Single compare-mode pass for harness-driven E2E. Expects WATCHDOG_* env vars
-- (see docs/watchdog/staging-drills.md). Exits 0 on match, 1 on failure.

package.path = "./?.lua;./?/init.lua;" .. package.path

local deps_lua = os.getenv("WATCHDOG_LUA_DEPS")
if deps_lua and deps_lua ~= "" then
    package.cpath = deps_lua .. "/?.so;" .. package.cpath
end

local config = require("watchdog.config")
local http_mod = require("watchdog.http")
local jsonrpc = require("watchdog.jsonrpc")
local machine_cli = require("watchdog.machine_cli")
local runner = require("watchdog.runner")
local sequencer_reader = require("watchdog.sequencer_reader")

local json = require("watchdog.json").new()

local function getenv_table()
    return setmetatable({}, {
        __index = function(_, key)
            return os.getenv(key)
        end,
    })
end

local cfg = config.load(getenv_table())
if cfg.mode ~= "compare" then
    io.stderr:write("WATCHDOG_MODE must be compare\n")
    os.exit(1)
end

local http = http_mod.new()
local deps = {
    http = http,
    rpc = jsonrpc.new(http, json, cfg.l1_rpc_url),
    sequencer = sequencer_reader.new(http, json, cfg.sequencer_url),
    -- Harness uses the CLI binding: it tracks `cartesi-machine` archive support
    -- for stored snapshots (the in-process `cartesi` module can lag the CLI).
    machine = machine_cli.new({
        executable = cfg.cm_executable,
        work_dir = cfg.cm_work_dir,
    }),
}

local result, err = runner.run_once(cfg, deps)
if not result then
    io.stderr:write("watchdog compare failed: " .. tostring(err) .. "\n")
    if type(err) == "table" then
        for key, value in pairs(err) do
            io.stderr:write(string.format("  %s: %s\n", tostring(key), tostring(value)))
        end
    end
    os.exit(1)
end

print(string.format(
    "watchdog compare ok: safe_block=%s input_count=%s",
    tostring(result.safe_block),
    tostring(result.input_count)
))
os.exit(0)
