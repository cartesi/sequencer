-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Watchdog commands (docs/watchdog/README.md#commands):
---
---   init    store a trusted bootstrap machine; persist the configuration
---   tick    one compare cycle; exit 0 ok/idle, 1 warning, 2 divergence latched
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

local function open_store(state_dir)
    local lfs = require("lfs")
    if lfs.attributes(state_dir, "mode") ~= "directory" then
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
    local store = open_store(cfg.state_dir)
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
    local archive = incident.clear(open_store(state_dir), block_flag(flags, "block"), flag(flags, "reason"), now())
    log("clear: incident archived at %s", archive)
    return EXIT_OK, { archived = archive }
end

function commands.replay(flags, env, factory)
    local cfg = read_config(open_store(config.state_dir(env)), env)
    local result = replay.run(cfg, { machine = factory.machine(), l1 = factory.l1(cfg) }, {
        from_dir = flag(flags, "from"),
        from_block = block_flag(flags, "from-block"),
        to_block = block_flag(flags, "to-block"),
        out_dir = flag(flags, "out"),
    })
    return EXIT_OK, result
end

--- Runs the tick and always records its outcome: `last_tick.json` for
--- `status`, and `status.prom` for alerting.
function commands.tick(_, env, factory)
    local report = { exit_code = EXIT_WARNING }
    local store
    local ok, err = pcall(function()
        store = open_store(config.state_dir(env))
        local cfg = read_config(store, env)
        report.chain_id, report.app_address = cfg.chain_id, cfg.app_address
        local outcome = tick.run(cfg, {
            store = store,
            machine = factory.machine(),
            sequencer = factory.sequencer(cfg),
            l1 = factory.l1(cfg),
            now = now,
        })
        report.outcome = outcome.kind
        if outcome.kind == "latched" or outcome.kind == "diverged" then
            report.exit_code = EXIT_DIVERGENCE
            report.divergence_kind = outcome.incident.kind
            if outcome.kind == "diverged" then
                io.stderr:write("watchdog_event " .. json.encode(outcome.incident) .. "\n")
            end
            log("tick: divergence %s latched at block %d; see `status`, runbook: docs/watchdog/incident-runbook.md",
                outcome.incident.kind, outcome.incident.target_block)
        else
            report.exit_code = EXIT_OK
            if outcome.kind == "agreed" then
                log("tick: sequencer agrees at block %d (from block %d)", outcome.head.block, outcome.previous.block)
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

    report.timestamp = os.time()
    if store then
        local head_ok, head = pcall(store.head, store)
        report.checked_block = head_ok and head and head.block or nil
        store:write_json({
            finished_at = os.date("!%Y-%m-%dT%H:%M:%SZ", report.timestamp),
            exit_code = report.exit_code,
            outcome = report.outcome,
            message = report.message,
            checked_block = report.checked_block,
        }, "last_tick.json")
    end
    local metrics_path = config.metrics_file(env) or (store and store:path("status.prom"))
    if metrics_path then
        store_mod.write_file_atomic(metrics_path, metrics.render(report))
    else
        log("tick: no state directory or CARTESI_WATCHDOG_METRICS_FILE; status.prom not written")
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
