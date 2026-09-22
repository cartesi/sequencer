-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Watchdog unit tests: every module against fakes at its boundary. The real
--- Cartesi Machine is exercised by tests/e2e.lua. Run from the repo root:
---   lua5.4 watchdog/tests/run.lua [name filter]

package.path = "./?.lua;./?/init.lua;" .. package.path
package.cpath = (os.getenv("CARTESI_WATCHDOG_LUA_DEPS") or ".deps/lua") .. "/?.so;" .. package.cpath

local abi = require("watchdog.abi")
local bootstrap = require("watchdog.bootstrap")
local config = require("watchdog.config")
local errors = require("watchdog.errors")
local incident = require("watchdog.incident")
local json = require("watchdog.json").new()
local l1_mod = require("watchdog.l1")
local metrics = require("watchdog.metrics")
local store_mod = require("watchdog.store")
local support = require("watchdog.tests.support")
local tick = require("watchdog.tick")

local runner = support.runner()
local test, eq, raises = runner.test, support.eq, support.raises

local function now()
    return "2026-09-22T12:00:00Z"
end

local function l1_reader(inputs, opts)
    local rpc = support.fake_rpc(inputs, opts)
    return l1_mod.new(rpc, {
        input_box_address = support.INPUT_BOX,
        app_address = support.APP,
        chain_id = support.CHAIN_ID,
    }), rpc
end

-- ── abi ───────────────────────────────────────────────────────────────────

test("abi decodes an InputAdded log and keeps the raw envelope", function()
    local fixture = dofile("watchdog/tests/fixtures/input_added_evm_advance.lua")
    local input = abi.decode_input_added_log(fixture.log)
    eq(input.app_contract, fixture.expected.app_contract)
    eq(input.msg_sender, fixture.expected.msg_sender)
    eq(input.block_number, fixture.expected.block_number)
    eq(input.chain_id, 31337)
    eq(input.index, 3)
    eq(abi.hex_from_bytes(input.payload), fixture.expected.payload_hex)
    eq(abi.decode_evm_advance(input.raw).block_number, 99)
end)

test("abi round-trips the test encoder", function()
    local raw = support.evm_advance({ block = 7, index = 5, payload = "hello" })
    local input = abi.decode_input_added_log(support.input_added_log(raw, 7))
    eq(input.raw, raw)
    eq(input.index, 5)
    eq(input.payload, "hello")
end)

-- ── l1 ────────────────────────────────────────────────────────────────────

local function collect(reader, from, to, first)
    local seen = {}
    local next_index = reader:inputs(from, to, first, function(input)
        seen[#seen + 1] = input
    end)
    return seen, next_index
end

test("l1 yields inputs in order and proves the count", function()
    local reader = l1_reader({
        { block = 3, payload = "a" },
        { block = 5, payload = "b" },
        { block = 9, payload = "c" },
    })
    local seen, next_index = collect(reader, 4, 9, 1)
    eq(#seen, 2)
    eq(seen[1].payload, "b")
    eq(seen[2].index, 2)
    eq(next_index, 3)
end)

test("l1 refuses a gap in InputBox indices", function()
    local reader = l1_reader({ { block = 3 }, { block = 5 }, { block = 9 } }, {
        drop = function(block)
            return block == 5
        end,
    })
    raises("transient", "index 2 where 1 was expected", function()
        collect(reader, 1, 9, 0)
    end)
end)

test("l1 refuses a truncated tail through the pinned count", function()
    local reader = l1_reader({ { block = 3 }, { block = 9 } }, {
        drop = function(block)
            return block == 9
        end,
    })
    raises("transient", "InputBox counts 2 inputs at block 9 but the scan reached index 1", function()
        collect(reader, 1, 9, 0)
    end)
end)

test("l1 refuses an input for another chain as an operator error", function()
    local reader = l1_reader({ { block = 3, chain_id = 1 } })
    raises("operator", "input for chain 1", function()
        collect(reader, 1, 3, 0)
    end)
end)

test("l1 stops early when asked and skips the count", function()
    local reader, rpc = l1_reader({ { block = 3 }, { block = 4 }, { block = 5 } })
    local next_index, stopped = reader:inputs(1, 5, 0, function(input)
        return input.index < 1
    end)
    eq(next_index, 2)
    eq(stopped, true)
    eq(#rpc.calls, 1)
end)

test("l1 counts no inputs before the InputBox exists, but refuses an empty address", function()
    local reader = l1_reader({ { block = 7 } }, { input_box_from = 5 })
    eq(reader:input_count_at(3), 0)
    eq(reader:input_count_at(7), 1)
    reader = l1_reader({}, { input_box_from = 5, input_box_code = "0x" })
    raises("operator", "no contract at the InputBox address", function()
        reader:input_count_at(3)
    end)
end)

test("l1 requires the safe head to reach the target", function()
    local reader = l1_reader({}, { safe_head = 10 })
    reader:require_safe_head(10)
    raises("transient", "safe head 10 is behind target block 11", function()
        reader:require_safe_head(11)
    end)
end)

test("l1 splits log ranges exactly like the shared Rust partition vector", function()
    local vector = json.decode(support.read("tests/fixtures/l1_partition_vector.json"))
    for i, code in ipairs(vector.long_block_range_error_codes) do
        eq(l1_mod.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES[i], code, "default code " .. i)
    end
    for _, scenario in ipairs(vector.scenarios) do
        local failures = {}
        for _, f in ipairs(scenario.fail_ranges) do
            failures[f.from .. ":" .. f.to] = f.message
        end
        local reader, rpc = l1_reader({}, {
            fail = function(lo, hi)
                return failures[lo .. ":" .. hi]
            end,
        })
        local ok = pcall(collect, reader, scenario.start_block, scenario.end_block, 0)
        eq(ok, scenario.expect_ok, scenario.name)
        eq(#rpc.calls, #scenario.expect_calls, scenario.name .. " calls")
        for i, call in ipairs(scenario.expect_calls) do
            eq(rpc.calls[i][1], call[1], scenario.name .. " call " .. i)
            eq(rpc.calls[i][2], call[2], scenario.name .. " call " .. i)
        end
    end
end)

-- ── config ────────────────────────────────────────────────────────────────

local function init_env(overrides)
    local env = {
        CARTESI_WATCHDOG_STATE_DIR = "/var/lib/watchdog",
        CARTESI_WATCHDOG_SEQUENCER_URL = "http://sequencer:3000",
        CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT = "http://l1:8545",
        CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS = "0x9999999999999999999999999999999999999999",
        CARTESI_WATCHDOG_APP_ADDRESS = "0xABCDEFabcdefABCDEFabcdefABCDEFabcdefABCD",
        CARTESI_WATCHDOG_STATE_SOURCE = "range:state",
        CARTESI_WATCHDOG_CM_SNAPSHOT_DIR = "/images/canonical",
        CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK = "0",
    }
    for key, value in pairs(overrides or {}) do
        env[key] = value ~= false and value or nil
    end
    return env
end

test("config reads init settings and normalizes addresses", function()
    local cfg = config.from_init_env(init_env())
    eq(cfg.app_address, "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd")
    eq(cfg.state_source.kind, "range")
    eq(cfg.state_source.label, "state")
    eq(cfg.bootstrap_block, 0)
    eq(cfg.chain_id, nil)
    eq(#cfg.long_block_range_error_codes, 5)
end)

test("config rejects relative paths, bad sources, and missing settings", function()
    raises("operator", "must be an absolute path", function()
        config.from_init_env(init_env({ CARTESI_WATCHDOG_STATE_DIR = "state" }))
    end)
    raises("operator", "`inspect` or `range:<label>`", function()
        config.from_init_env(init_env({ CARTESI_WATCHDOG_STATE_SOURCE = "drive" }))
    end)
    raises("operator", "CARTESI_WATCHDOG_STATE_SOURCE is required", function()
        config.from_init_env(init_env({ CARTESI_WATCHDOG_STATE_SOURCE = false }))
    end)
end)

test("config round-trips through config.json and keeps endpoints at run time", function()
    local cfg = config.from_init_env(init_env({ CARTESI_WATCHDOG_BLOCKCHAIN_ID = "31337" }))
    local document = json.decode(json.encode(config.persisted(cfg)))
    local loaded = config.load("/var/lib/watchdog", document, {
        CARTESI_WATCHDOG_SEQUENCER_URL = "http://rotated:3000",
    })
    eq(loaded.chain_id, 31337)
    eq(loaded.sequencer_url, "http://rotated:3000")
    eq(loaded.rpc_url, nil)
    eq(config.format_state_source(loaded.state_source), "range:state")
    assert(config.same_identity(document, config.persisted(cfg)))
    raises("operator", "not a version 2 watchdog config", function()
        document.version = 1
        config.load("/var/lib/watchdog", document, {})
    end)
end)

-- ── store ─────────────────────────────────────────────────────────────────

test("store finds the newest checkpoint as head and refuses stray entries", function()
    local store = support.state_dir(json, 5, 2)
    assert(require("lfs").mkdir(store:checkpoint_dir(12, 4)))
    local head = store:head()
    eq(head.block, 12)
    eq(head.input_count, 4)
    eq(#store:checkpoints(), 2)
    assert(require("lfs").mkdir(store:path("checkpoints", "junk")))
    raises("operator", "unexpected entry junk", function()
        store:head()
    end)
end)

test("store resets scratch space and writes JSON atomically", function()
    local store = support.state_dir(json, 0, 0)
    support.write(store:reset_work() .. "/leftover", "x")
    store:reset_work()
    eq(support.exists(store:path("work", "leftover")), false)
    store:write_json({ a = 1 }, "x.json")
    eq(store:read_json("x.json").a, 1)
    eq(support.exists(store:path("x.json.tmp")), false)
end)

-- ── incident ──────────────────────────────────────────────────────────────

test("incident compares evidence files by offset and page", function()
    local dir = support.tmpdir()
    local a = string.rep("\0", 10000)
    local b = a:sub(1, 5000) .. "x" .. a:sub(5002, 9000) .. "y" .. a:sub(9002)
    support.write(dir .. "/a", a)
    support.write(dir .. "/b", b)
    local result = incident.compare_files(dir .. "/a", dir .. "/b")
    eq(result.first_difference, 5000)
    eq(result.differing_pages, 2)
    support.write(dir .. "/c", a)
    eq(incident.compare_files(dir .. "/a", dir .. "/c"), nil)
    support.write(dir .. "/short", a:sub(1, 100))
    eq(incident.compare_files(dir .. "/a", dir .. "/short").first_difference, 100)
end)

test("incident clear archives only the named block's incident", function()
    local store = support.state_dir(json, 0, 0)
    incident.latch(store, { kind = "state_mismatch", target_block = 7 }, {}, now())
    raises("operator", "is for block 7, not 8", function()
        incident.clear(store, 8, "wrong block", now())
    end)
    local archive = incident.clear(store, 7, "drill", now())
    eq(incident.marker(store), nil)
    eq(json.decode(support.read(archive .. "/resolution.json")).reason, "drill")
    raises("operator", "no divergence is latched", function()
        incident.clear(store, 7, "again", now())
    end)
end)

test("incident discards an interrupted latch but keeps a complete one", function()
    local store = support.state_dir(json, 0, 0)
    store:mkdir("incident")
    incident.discard_interrupted(store)
    eq(store:exists("incident"), false)
    incident.latch(store, { kind = "state_mismatch", target_block = 1 }, {}, now())
    incident.discard_interrupted(store)
    eq(incident.marker(store).target_block, 1)
end)

-- ── tick ──────────────────────────────────────────────────────────────────

local function tick_with(store, inputs, sequencer_state, script, rpc_opts)
    local reader = l1_reader(inputs, rpc_opts)
    return tick.run({ chain_id = support.CHAIN_ID, state_source = { kind = "range", label = "state" } }, {
        store = store,
        machine = support.fake_machine(script),
        sequencer = support.fake_sequencer(sequencer_state),
        l1 = reader,
        now = now,
    })
end

-- The fake machine appends each accepted payload's last byte to its state.
local function payloads(...)
    local inputs = {}
    for i, payload in ipairs({ ... }) do
        inputs[i] = { block = i + 10, payload = payload }
    end
    return inputs
end

test("tick is idle while the sequencer stays at the head", function()
    local store = support.state_dir(json, 11, 1, "a")
    eq(tick_with(store, payloads("a"), { block = 11 }).kind, "idle")
end)

test("tick replays to the digest's block and advances the head on agreement", function()
    local store = support.state_dir(json, 11, 1, "a")
    local outcome = tick_with(store, payloads("a", "b", "c"), { polled = 12, block = 13, sha256 = "abc" })
    eq(outcome.kind, "agreed")
    eq(outcome.head.block, 13)
    eq(outcome.head.input_count, 3)
    eq(#store:checkpoints(), 1)
    eq(store:head().block, 13)
    eq(support.read(store:head().dir .. "/state"), "abc")
end)

test("tick latches a state mismatch with evidence and keeps the head", function()
    local store = support.state_dir(json, 11, 1, "a")
    local outcome = tick_with(store, payloads("a", "b"), { block = 12, sha256 = "aX", bytes = "aX" })
    eq(outcome.kind, "diverged")
    local marker = incident.marker(store)
    eq(marker.kind, "state_mismatch")
    eq(marker.target_block, 12)
    eq(marker.agreed.block, 11)
    eq(marker.canonical_sha256, "ab")
    eq(marker.evidence.comparison.first_difference, 1)
    eq(support.read(store:path("incident", "canonical", "state")), "ab")
    eq(store:head().block, 11)
    eq(tick_with(store, payloads("a", "b"), { block = 12, sha256 = "ab" }).kind, "latched")
end)

test("tick records evidence as missing when the sequencer moved on", function()
    local store = support.state_dir(json, 11, 1, "a")
    tick_with(store, payloads("a", "b"), { block = 12, sha256 = "zz", served_block = 13 })
    eq(incident.marker(store).evidence.sequencer_bytes_missing, "the sequencer moved on to block 13")
    eq(support.exists(store:path("incident", "sequencer.bin")), false)
end)

test("tick latches a dead canonical machine at the input that stopped it", function()
    local store = support.state_dir(json, 11, 1, "a")
    local outcome = tick_with(store, payloads("a", "b", "c"), { block = 13, sha256 = "abc" }, {
        statuses = { { kind = "halted", exit_code = 7 } },
    })
    eq(outcome.kind, "diverged")
    local marker = incident.marker(store)
    eq(marker.kind, "canonical_machine_dead")
    eq(marker.stop.status, "halted")
    eq(marker.stop.input_index, 1)
    eq(marker.stop.input_block, 12)
end)

test("tick continues past a rejected input", function()
    local store = support.state_dir(json, 11, 1, "a")
    local outcome = tick_with(store, payloads("a", "b", "c"), { block = 13, sha256 = "ac" }, {
        statuses = { { kind = "rejected" } },
    })
    eq(outcome.kind, "agreed")
end)

test("tick latches an inclusion block regression", function()
    local store = support.state_dir(json, 11, 1, "a")
    eq(tick_with(store, payloads("a"), { block = 10 }).kind, "diverged")
    eq(incident.marker(store).kind, "inclusion_block_regressed")
end)

test("tick refuses an RPC for another chain and a missing head", function()
    local store = support.state_dir(json, 11, 1, "a")
    raises("operator", "does not serve chain 31337", function()
        tick_with(store, payloads("a", "b"), { block = 12, sha256 = "ab" }, nil, { chain_id = 1 })
    end)
    store:remove("checkpoints")
    raises("operator", "run init", function()
        tick_with(store, payloads("a"), { block = 12 })
    end)
end)

-- ── bootstrap ─────────────────────────────────────────────────────────────

local function bootstrap_with(store, env, inputs, script, rpc_opts)
    local cfg = config.from_init_env(init_env(env))
    cfg.state_dir = store.dir
    local image = support.tmpdir()
    support.write(image .. "/state", "genesis")
    cfg.bootstrap_dir = image
    return bootstrap.run(cfg, {
        store = store,
        machine = support.fake_machine(script),
        l1 = l1_reader(inputs or {}, rpc_opts),
    })
end

test("bootstrap stores the bootstrap machine and persists the config", function()
    local store = store_mod.open(support.tmpdir(), json)
    local result = bootstrap_with(store, { CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK = "5" }, { { block = 3 } })
    eq(result.kind, "initialized")
    eq(store:head().block, 5)
    eq(store:head().input_count, 1)
    eq(store:read_json("config.json").chain_id, support.CHAIN_ID)
    eq(bootstrap_with(store, { CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK = "5" }, { { block = 3 } }).kind,
        "already_initialized")
end)

test("bootstrap refuses a different deployment on an initialized directory", function()
    local store = store_mod.open(support.tmpdir(), json)
    bootstrap_with(store, {})
    raises("operator", "initialized for another deployment or state source", function()
        bootstrap_with(store, { CARTESI_WATCHDOG_STATE_SOURCE = "inspect" })
    end)
end)

test("bootstrap requires the template when no input precedes the block", function()
    local store = store_mod.open(support.tmpdir(), json)
    raises("operator", "differs from the on-chain template hash", function()
        bootstrap_with(store, {}, {}, { root_hash = string.rep("\1", 32) })
    end)
    eq(store:exists("config.json"), false)
end)

test("bootstrap refuses a pinned chain id the RPC does not serve", function()
    local store = store_mod.open(support.tmpdir(), json)
    raises("operator", "CARTESI_WATCHDOG_BLOCKCHAIN_ID is 5", function()
        bootstrap_with(store, { CARTESI_WATCHDOG_BLOCKCHAIN_ID = "5" })
    end)
end)

-- ── metrics ───────────────────────────────────────────────────────────────

test("metrics match the golden status files", function()
    local report = {
        chain_id = 11155111,
        app_address = "0x4ce633ca2f0dd4b4ce6d2a1bcf0b8bd0db0f0ba0",
        timestamp = 1790000000,
        checked_block = 123,
    }
    report.exit_code = 0
    eq(metrics.render(report), support.read("tests/fixtures/watchdog_status_ok.prom"))
    report.exit_code, report.divergence_kind = 2, "state_mismatch"
    eq(metrics.render(report), support.read("tests/fixtures/watchdog_status_failed.prom"))
end)

test("metrics label an unknown deployment when the config is unreadable", function()
    local text = metrics.render({ exit_code = 1, timestamp = 1 })
    assert(text:find('cartesi_watchdog_status{app_address="unknown",chain="unknown",state="warning"} 1', 1, true))
end)

-- ── main ──────────────────────────────────────────────────────────────────

--- A stdout stand-in collecting what a command prints.
local function sink()
    local chunks = {}
    return {
        write = function(_, ...)
            for _, chunk in ipairs({ ... }) do
                chunks[#chunks + 1] = chunk
            end
        end,
        text = function()
            return table.concat(chunks)
        end,
    }
end

local function main_factory(inputs, sequencer_state, script)
    return {
        machine = function()
            return support.fake_machine(script)
        end,
        l1 = function()
            return (l1_reader(inputs))
        end,
        sequencer = function()
            return support.fake_sequencer(sequencer_state)
        end,
    }
end

test("main tick exits 0/2 and records last_tick.json and status.prom", function()
    local main = require("watchdog.main")
    local store = support.state_dir(json, 11, 1, "a")
    store:write_json(config.persisted(config.from_init_env(init_env({ CARTESI_WATCHDOG_BLOCKCHAIN_ID = "31337" }))),
        "config.json")
    local env = { CARTESI_WATCHDOG_STATE_DIR = store.dir }

    local function run(argv, factory, stdout)
        return main.run(argv, { env = env, factory = factory, stdout = stdout })
    end
    eq(run({ "tick" }, main_factory(payloads("a", "b"), { block = 12, sha256 = "ab" })), 0)
    eq(store:read_json("last_tick.json").outcome, "agreed")
    assert(support.read(store:path("status.prom")):find('state="ok"} 1', 1, true))

    eq(run({ "tick" }, main_factory(payloads("a", "b", "c"), { block = 13, sha256 = "abX" })), 2)
    assert(support.read(store:path("status.prom")):find('kind="state_mismatch"} 1', 1, true))
    eq(run({ "tick" }, main_factory({}, {})), 2)
    eq(store:read_json("last_tick.json").outcome, "latched")

    local out = sink()
    eq(run({ "status" }, nil, out), 0)
    local status = json.decode(out:text())
    eq(status.latched, true)
    eq(status.head.block, 12)
    eq(status.divergence.target_block, 13)

    eq(run({ "clear", "--block", "12", "--reason", "drill" }), 1)
    eq(run({ "clear", "--block=13", "--reason=drill" }, nil, sink()), 0)
    eq(incident.marker(store), nil)
end)

test("main tick without a state directory exits 1 and still reports", function()
    local main = require("watchdog.main")
    local metrics_file = support.tmpdir() .. "/status.prom"
    eq(main.run({ "tick" }, { env = { CARTESI_WATCHDOG_METRICS_FILE = metrics_file }, factory = main_factory({}, {}) }),
        1)
    assert(support.read(metrics_file):find('state="warning"} 1', 1, true))
end)

test("errors classify raised failures", function()
    local _, err = pcall(errors.transient, "rpc %d", 5)
    eq(select(1, errors.classify(err)), "transient")
    eq(select(2, errors.classify(err)), "rpc 5")
    _, err = pcall(error, "boom")
    eq(select(1, errors.classify(err)), "internal")
end)

os.exit(runner.run(arg[1]) and 0 or 1)
