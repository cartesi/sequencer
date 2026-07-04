-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

package.path = "./?.lua;./?/init.lua;" .. package.path

local config = require("watchdog.config")
local checkpoint = require("watchdog.checkpoint")
local http_mod = require("watchdog.http")
local json_mod = require("watchdog.json")
local jsonrpc = require("watchdog.jsonrpc")
local machine_cartesi = require("watchdog.machine_cartesi")
local metrics = require("watchdog.metrics")
local retry = require("watchdog.retry")
local runner = require("watchdog.runner")
local sequencer_reader = require("watchdog.sequencer_reader")
local state = require("watchdog.state")

local EXIT_OK = 0
local EXIT_TRANSIENT = 1
local EXIT_DIVERGENCE = 2

local function prepend_deps_cpath()
    local deps = os.getenv("CARTESI_WATCHDOG_LUA_DEPS")
    if deps and deps ~= "" then
        package.cpath = deps .. "/?.so;" .. package.cpath
    end
end

local function load_machine(_cfg)
    return machine_cartesi.new()
end

local function log_step(message)
    io.stderr:write("watchdog_step " .. tostring(message) .. "\n")
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
end

local function emit_watchdog_event(json, payload, deps)
    if deps and type(deps.on_watchdog_event) == "function" then
        deps.on_watchdog_event(payload)
    end
    io.stderr:write("watchdog_event " .. json.encode(payload) .. "\n")
end

local function divergence_kind_from_payload(payload)
    if type(payload) == "table" and type(payload.kind) == "string" then
        return payload.kind
    end
    return nil
end

local function write_tick_metrics(cfg, exit_code, payload, env)
    local ok, err = metrics.write_tick_status({
        cfg = cfg,
        env = env or os.getenv,
        exit_code = exit_code,
        chain_id = cfg and cfg.blockchain_id or nil,
        app_address = cfg and cfg.app_address or nil,
        divergence_kind = divergence_kind_from_payload(payload),
        timestamp = os.time(),
    })
    if not ok then
        io.stderr:write("watchdog metrics write failed: " .. tostring(err) .. "\n")
    end
end

local function default_machine_deps(cfg)
    return {
        machine = load_machine(cfg),
        log_step = log_step,
    }
end

local function default_deps(cfg)
    local http = http_mod.new()
    local json = json_mod.new()
    local deps = default_machine_deps(cfg)
    deps.http = http
    deps.rpc = jsonrpc.new(http, json, cfg.l1_rpc_url)
    deps.sequencer = sequencer_reader.new(http, json, cfg.sequencer_url)
    return deps, json
end

local function run_init(cfg, deps)
    deps = deps or default_machine_deps(cfg)
    local json = json_mod.new()

    local existing, load_err = checkpoint.load(cfg.state_dir)
    if existing then
        return nil, "watchdog state already initialized"
    end
    if load_err ~= "missing " .. checkpoint.HEAD_FILE then
        return nil, "failed to load watchdog head: " .. tostring(load_err)
    end

    local ok, err = state.write_json_atomic(cfg.state_dir, "config.json", config.persisted(cfg), json)
    if not ok then
        return nil, err
    end

    local machine = deps.machine or load_machine(cfg)
    if type(deps.log_step) == "function" then
        deps.log_step("load bootstrap CM snapshot")
    end
    local instance, load_snapshot_err = machine:load(cfg.cm_snapshot_dir, cfg.cm_snapshot_safe_block)
    if not instance then
        return nil, load_snapshot_err
    end

    if type(deps.log_step) == "function" then
        deps.log_step("persist initial watchdog checkpoint")
    end
    local written, write_err = checkpoint.write(cfg.state_dir, cfg.cm_snapshot_safe_block, function(snapshot_dir)
        return machine:dump(instance, snapshot_dir, cfg.cm_snapshot_safe_block)
    end, {
        created_at = os.date("!%Y-%m-%dT%H:%M:%SZ"),
        cm_image_hash = cfg.cm_image_hash,
    })
    if not written then
        return nil, write_err
    end

    return {
        ok = true,
        safe_block = cfg.cm_snapshot_safe_block,
    }
end

local function load_tick_config(env)
    local json = json_mod.new()
    local state_dir = config.load_state_dir(env)
    local persisted, err = state.read_json(state_dir, "config.json", json)
    if not persisted then
        error("failed to load config.json: " .. tostring(err))
    end
    return config.from_persisted(state_dir, persisted, env)
end

local function run_compare_cycle(cfg, deps)
    deps = deps or select(1, default_deps(cfg))
    local json = json_mod.new()
    local result, err = retry.with_retries(function()
        local ok, value, run_err = pcall(runner.run_once, cfg, deps)
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
            emit_watchdog_event(json, err, deps)
            return EXIT_DIVERGENCE, err
        end
        return EXIT_TRANSIENT, err
    end
    return EXIT_OK, result
end

local function run_tick(cfg, deps)
    return run_compare_cycle(cfg, deps)
end

local function usage()
    return "usage: watchdog <init|tick>"
end

local function load_or_error(loader)
    local ok, value = pcall(loader)
    if not ok then
        return nil, value
    end
    return value
end

local function exit_for_result(command, exit_code, err, cfg, env)
    if command == "tick" then
        write_tick_metrics(cfg, exit_code, err, env)
    end
    if exit_code == EXIT_DIVERGENCE then
        os.exit(EXIT_DIVERGENCE)
    end
    if exit_code == EXIT_TRANSIENT then
        io.stderr:write("watchdog " .. command .. " failed: " .. tostring(err) .. "\n")
        os.exit(EXIT_TRANSIENT)
    end
    os.exit(EXIT_OK)
end

-- One compare cycle per `tick` process. Infra (systemd timer / k8s CronJob)
-- schedules re-runs and reacts to status.prom / the exit code; the watchdog
-- itself does not loop.
local function main(argv, opts)
    argv = argv or arg or {}
    opts = opts or {}
    local injected_env = opts.env
    local injected_deps = opts.deps
    prepend_deps_cpath()

    local command = argv[1]
    if command == "init" then
        local cfg, cfg_err = load_or_error(function()
            return config.load_init()
        end)
        if not cfg then
            io.stderr:write("watchdog init failed: " .. tostring(cfg_err) .. "\n")
            os.exit(EXIT_TRANSIENT)
        end
        local ok, result, err = pcall(run_init, cfg)
        if not ok then
            io.stderr:write("watchdog init failed: " .. tostring(result) .. "\n")
            os.exit(EXIT_TRANSIENT)
        end
        if not result then
            io.stderr:write("watchdog init failed: " .. tostring(err) .. "\n")
            os.exit(EXIT_TRANSIENT)
        end
        os.exit(EXIT_OK)
    end

    if command == "tick" then
        local cfg, cfg_err = load_or_error(function()
            return load_tick_config(injected_env)
        end)
        if not cfg then
            io.stderr:write("watchdog tick failed: " .. tostring(cfg_err) .. "\n")
            write_tick_metrics(nil, EXIT_TRANSIENT, cfg_err, injected_env)
            os.exit(EXIT_TRANSIENT)
        end
        local ok, exit_code, err = pcall(run_tick, cfg, injected_deps)
        if not ok then
            io.stderr:write("watchdog tick failed: " .. tostring(exit_code) .. "\n")
            write_tick_metrics(cfg, EXIT_TRANSIENT, exit_code, injected_env)
            os.exit(EXIT_TRANSIENT)
        end
        if not exit_code then
            exit_code = EXIT_TRANSIENT
        end
        exit_for_result(command, exit_code, err, cfg, injected_env)
    end

    io.stderr:write(usage() .. "\n")
    os.exit(EXIT_TRANSIENT)
end

local function invoked_as_script()
    return type(arg) == "table"
        and type(arg[0]) == "string"
        and arg[0]:match("watchdog[/\\]main%.lua$") ~= nil
end

if invoked_as_script() then
    main(arg)
end

return {
    main = main,
    run_init = run_init,
    run_tick = run_tick,
    run_compare_cycle = run_compare_cycle,
    load_tick_config = load_tick_config,
    EXIT_OK = EXIT_OK,
    EXIT_TRANSIENT = EXIT_TRANSIENT,
    EXIT_DIVERGENCE = EXIT_DIVERGENCE,
    write_tick_metrics = write_tick_metrics,
}
