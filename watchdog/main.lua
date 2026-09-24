-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Watchdog commands (docs/watchdog/README.md#commands):
---
---   init    store a trusted bootstrap machine; persist the configuration
---   tick    one compare cycle; exit 0 ok/idle, 1 warning, 2 divergence
---   status  JSON summary of the state directory
---   clear   --block <B> --reason <text>: archive the latched divergence at B
---   replay  --from <dir> --from-block <A> --to-block <B> --out <dir>:
---           re-derive canonical state into a new directory

package.path = "./?.lua;./?/init.lua;" .. package.path
do
    local deps_dir = os.getenv("CARTESI_WATCHDOG_LUA_DEPS")
    if deps_dir and deps_dir ~= "" then
        package.cpath = deps_dir .. "/?.so;" .. package.cpath
    end
end

local bootstrap = require("watchdog.bootstrap")
local config = require("watchdog.config")
local errors = require("watchdog.errors")
local incident = require("watchdog.incident")
local json = require("watchdog.json").new()
local metrics = require("watchdog.metrics")
local replay = require("watchdog.replay")
local store_mod = require("watchdog.store")
local tick = require("watchdog.tick")

local EXIT_OK, EXIT_WARNING, EXIT_DIVERGENCE = 0, 1, 2

local function now()
    return os.date("!%Y-%m-%dT%H:%M:%SZ")
end

local function log(fmt, ...)
    io.stderr:write("watchdog: " .. string.format(fmt, ...) .. "\n")
end

--- Production collaborators; tests substitute their own.
local function real_factory()
    local http
    local factory = {}
    function factory.http()
        http = http or require("watchdog.http").new()
        return http
    end
    function factory.machine()
        return require("watchdog.machine").new()
    end
    function factory.l1(cfg)
        if not cfg.rpc_url then
            errors.operator("CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT is required")
        end
        local rpc = require("watchdog.jsonrpc").new(factory.http(), json, cfg.rpc_url)
        return require("watchdog.l1").new(rpc, cfg)
    end
    function factory.sequencer(cfg)
        return require("watchdog.sequencer").new(factory.http(), json, cfg.sequencer_url)
    end
    return factory
end

local function parse_flags(argv)
    local flags = {}
    local i = 2
    while i <= #argv do
        local key, value = argv[i]:match("^%-%-([%w-]+)=(.*)$")
        if not key then
            key = argv[i]:match("^%-%-([%w-]+)$")
            if not key or argv[i + 1] == nil then
                errors.operator("unexpected argument %q", argv[i])
            end
            i = i + 1
            value = argv[i]
        end
        flags[key] = value
        i = i + 1
    end
    return flags
end

local function flag(flags, name)
    return flags[name] or errors.operator("--%s is required", name)
end

local function block_flag(flags, name)
    local value = math.tointeger(tonumber(flag(flags, name)))
    if not value or value < 0 then
        errors.operator("--%s must be a non-negative integer", name)
    end
    return value
end

--- The state directory; only commands that write it may create it, so a
--- mistyped path fails `status` and `replay` instead of creating a directory.
local function open_store(state_dir, create)
    local lfs = require("lfs")
    if lfs.attributes(state_dir, "mode") ~= "directory" then
        if not create then
            errors.operator("state directory %s does not exist", state_dir)
        end
        local ok, err = lfs.mkdir(state_dir)
        if not ok then
            errors.operator("cannot create state directory %s: %s", state_dir, tostring(err))
        end
    end
    return store_mod.open(state_dir, json)
end

local function read_config(store, env)
    local document = store:read_json("config.json")
    if not document then
        errors.operator("%s is not initialized; run init", store.dir)
    end
    return config.load(store.dir, document, env)
end

local commands = {}

function commands.init(_, env, factory)
    local cfg = config.from_init_env(env)
    local store = open_store(cfg.state_dir, true)
    local result = bootstrap.run(cfg, { store = store, machine = factory.machine(), l1 = factory.l1(cfg) })
    log("init: %s; head at block %d (%d inputs)", result.kind, result.head.block, result.head.input_count)
    return EXIT_OK
end

function commands.status(_, env)
    local state_dir = config.state_dir(env)
    local store = open_store(state_dir)
    local document = store:read_json("config.json")
    local head = store:head()
    local marker = incident.marker(store)
    if marker then
        marker.evidence = incident.evidence(store)
    end
    return EXIT_OK, {
        state_dir = state_dir,
        initialized = document ~= nil,
        config = document,
        head = head and { block = head.block, input_count = head.input_count },
        latched = marker ~= nil,
        divergence = marker,
        last_tick = store:read_json("last_tick.json"),
    }
end

function commands.clear(flags, env)
    local state_dir = config.state_dir(env)
    local store = open_store(state_dir, true)
    local archive = incident.clear(store, block_flag(flags, "block"), flag(flags, "reason"), now())
    log("clear: incident archived at %s", archive)
    return EXIT_OK, { archived = archive }
end

function commands.replay(flags, env, factory)
    local cfg = read_config(open_store(config.state_dir(env)), env)
    local result = replay.run(cfg, { machine = factory.machine(), l1 = factory.l1(cfg) }, {
        from_dir = config.absolute_path(flag(flags, "from"), "--from"),
        from_block = block_flag(flags, "from-block"),
        to_block = block_flag(flags, "to-block"),
        out_dir = config.absolute_path(flag(flags, "out"), "--out"),
    })
    return EXIT_OK, result
end

--- Record a tick's outcome: `last_tick.json` for `status`, and
--- `status.prom` for alerting. Recording must never change the outcome.
local function record(store, env, report)
    report.timestamp = os.time()
    if store then
        local head_ok, head = pcall(store.head, store)
        report.head_block = head_ok and head and head.block or nil
        local written, write_err = pcall(store.write_json, store, {
            finished_at = os.date("!%Y-%m-%dT%H:%M:%SZ", report.timestamp),
            exit_code = report.exit_code,
            outcome = report.outcome,
            message = report.message,
            head_block = report.head_block,
        }, "last_tick.json")
        if not written then
            log("tick: cannot write last_tick.json: %s", select(2, errors.classify(write_err)))
        end
    end
    local path_ok, metrics_path = pcall(config.metrics_file, env)
    if not path_ok then
        log("tick: %s; writing the state directory's status.prom instead", select(2, errors.classify(metrics_path)))
        metrics_path = nil
    end
    metrics_path = metrics_path or (store and store:path("status.prom"))
    if metrics_path then
        local written, write_err = pcall(store_mod.write_file_atomic, metrics_path, metrics.render(report))
        if not written then
            log("tick: cannot write %s: %s", metrics_path, select(2, errors.classify(write_err)))
        end
    else
        log("tick: no state directory or CARTESI_WATCHDOG_METRICS_FILE; status.prom not written")
    end
end

--- Runs the tick and records its outcome. A new divergence is latched, and
--- its event, `last_tick.json`, and `status.prom` are written, before its
--- evidence is collected: a slow or failed collection can delay none of them
--- and never downgrades the exit code. The process exits once it ends.
function commands.tick(_, env, factory)
    local report = { exit_code = EXIT_WARNING }
    local store, diverged
    local ok, err = pcall(function()
        store = open_store(config.state_dir(env), true)
        -- A latch holds even when nothing else can run.
        local latched = incident.marker(store)
        local cfg_ok, cfg = pcall(read_config, store, env)
        if cfg_ok then
            report.chain_id, report.app_address = cfg.chain_id, cfg.app_address
        end
        local outcome
        if latched then
            outcome = { kind = "latched", incident = latched }
            local marked, interrupted = pcall(incident.mark_interrupted, store)
            if marked and interrupted then
                log("tick: the latching tick's evidence collection was interrupted; see `status`")
            end
        elseif not cfg_ok then
            error(cfg, 0)
        else
            outcome = tick.run(cfg, {
                store = store,
                machine = factory.machine(),
                sequencer = factory.sequencer(cfg),
                l1 = factory.l1(cfg),
                now = now,
            })
        end
        report.outcome = outcome.kind
        if outcome.kind == "latched" or outcome.kind == "diverged" then
            report.exit_code = EXIT_DIVERGENCE
            report.divergence_kind = outcome.incident.kind
            if outcome.kind == "diverged" then
                diverged = outcome
                io.stderr:write("watchdog_event " .. json.encode(outcome.incident) .. "\n")
            end
            if outcome.latch_error then
                report.message = "divergence not latched: " .. outcome.latch_error
                log("tick: divergence %s detected at block %s but NOT latched: %s; runbook: "
                    .. "docs/watchdog/incident-runbook.md", outcome.incident.kind,
                    tostring(outcome.incident.target_block), outcome.latch_error)
            else
                log("tick: divergence %s latched at block %s; see `status`, runbook: "
                    .. "docs/watchdog/incident-runbook.md", outcome.incident.kind,
                    tostring(outcome.incident.target_block))
            end
            if outcome.record_error then
                report.message = "latch record not written: " .. outcome.record_error
                log("tick: the latch record could not be written, so `status` shows kind unreadable_marker: %s",
                    outcome.record_error)
            end
        else
            report.exit_code = EXIT_OK
            if outcome.kind == "agreed" then
                log("tick: sequencer agrees at block %d (from block %d)", outcome.head.block, outcome.previous.block)
            elseif outcome.sequencer_block then
                log("tick: idle at bootstrap block %d; the sequencer's accepted block %d has not reached it",
                    outcome.head.block, outcome.sequencer_block)
            else
                log("tick: idle at block %d", outcome.head.block)
            end
        end
    end)
    if not ok then
        local class, message = errors.classify(err)
        report.outcome, report.message = class, message
        log("tick: %s failure: %s", class, message)
    end
    record(store, env, report)

    if diverged and diverged.evidence then
        local collected, summary = pcall(function()
            return json.encode(incident.collect(store, diverged.incident, diverged.evidence))
        end)
        log("tick: evidence for block %s: %s", tostring(diverged.incident.target_block),
            collected and summary or select(2, errors.classify(summary)))
    end
    return report.exit_code
end

local main = {}

--- Run one command; returns its exit code. `opts` (for tests): env,
--- factory, stdout.
function main.run(argv, opts)
    opts = opts or {}
    local name = argv[1]
    local command = commands[name]
    if not command then
        log("usage: sequencer-watchdog <init|tick|status|clear|replay> [flags]")
        return EXIT_WARNING
    end
    local ok, exit_code, output = pcall(function()
        return command(parse_flags(argv), opts.env, opts.factory or real_factory())
    end)
    if not ok then
        local class, message = errors.classify(exit_code)
        log("%s: %s failure: %s", name, class, message)
        return EXIT_WARNING
    end
    if output ~= nil then
        (opts.stdout or io.stdout):write(json.encode(output), "\n")
    end
    return exit_code
end

if type(arg) == "table" and type(arg[0]) == "string" and arg[0]:match("watchdog[/\\]main%.lua$") then
    os.exit(main.run(arg), true)
end

return main
