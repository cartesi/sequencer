-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

package.path = "./?.lua;./?/init.lua;" .. package.path

local abi = require("watchdog.abi")
local checkpoint = require("watchdog.checkpoint")
local compare = require("watchdog.compare")
local config = require("watchdog.config")
local jsonrpc = require("watchdog.jsonrpc")
local l1_reader = require("watchdog.l1_reader")
local main_mod = require("watchdog.main")
local metrics = require("watchdog.metrics")
local retry = require("watchdog.retry")
local runner = require("watchdog.runner")
local sequencer_reader = require("watchdog.sequencer_reader")
local state_mod = require("watchdog.state")

local tests = {}

local function test(name, fn)
    table.insert(tests, { name = name, fn = fn })
end

local function assert_eq(actual, expected)
    if actual ~= expected then
        error(string.format("expected %q, got %q", tostring(expected), tostring(actual)), 2)
    end
end

local TEST_OS_EXIT = "__TEST_OS_EXIT__"

local function capture_os_exit(fn)
    local captured = nil
    local original_exit = os.exit
    os.exit = function(code)
        captured = code or 0
        error(TEST_OS_EXIT)
    end
    local ok, err = pcall(fn)
    os.exit = original_exit
    if not ok then
        if tostring(err):find(TEST_OS_EXIT, 1, true) then
            return captured, nil
        end
        return nil, err
    end
    return captured, nil
end

test("raw compare fails byte-different JSON", function()
    local ok, offset = compare.raw_equal('{"a":1}', '{ "a": 1 }')
    assert_eq(ok, false)
    assert(offset ~= nil, "expected mismatch offset")
end)

test("decodes InputAdded log EvmAdvance envelope", function()
    local fixture = dofile("watchdog/tests/fixtures/input_added_evm_advance.lua")
    local decoded = abi.decode_input_added_log(fixture.log)
    assert_eq(decoded.app_contract, fixture.expected.app_contract)
    assert_eq(decoded.msg_sender, fixture.expected.msg_sender)
    assert_eq(decoded.block_number, fixture.expected.block_number)
    assert_eq(abi.hex_from_bytes(decoded.payload), fixture.expected.payload_hex)
    assert(decoded.raw_input ~= nil and #decoded.raw_input > 0, "fixture keeps raw input bytes")
end)

test("sorts logs in L1 order", function()
    local logs = {
        { blockNumber = "0x2", transactionIndex = "0x0", logIndex = "0x5" },
        { blockNumber = "0x1", transactionIndex = "0x9", logIndex = "0x0" },
        { blockNumber = "0x2", transactionIndex = "0x0", logIndex = "0x1" },
    }
    l1_reader.sort_logs(logs)
    assert_eq(logs[1].blockNumber, "0x1")
    assert_eq(logs[2].logIndex, "0x1")
    assert_eq(logs[3].logIndex, "0x5")
end)

local function load_partition_vector()
    local json = require("watchdog.json").new()
    local path = "tests/fixtures/l1_partition_vector.json"
    local file, err = io.open(path, "rb")
    if not file then
        error("open " .. path .. ": " .. tostring(err))
    end
    local body = file:read("*a")
    file:close()
    return json.decode(body)
end

local function fail_lookup(fail_ranges)
    local map = {}
    for _, entry in ipairs(fail_ranges) do
        map[entry.from .. ":" .. entry.to] = entry.message
    end
    return map
end

local function load_wallet_snapshot_hex_fixture()
    local path = "tests/fixtures/wallet_snapshot_empty.hex"
    local file, err = io.open(path, "rb")
    if not file then
        error("open " .. path .. ": " .. tostring(err))
    end
    local hex = file:read("*a"):gsub("%s+", "")
    file:close()
    local bytes = {}
    for i = 1, #hex, 2 do
        table.insert(bytes, string.char(tonumber(hex:sub(i, i + 1), 16)))
    end
    return table.concat(bytes)
end

test("wallet SSZ golden fixture loads for cross-stack parity", function()
    local bytes = load_wallet_snapshot_hex_fixture()
    assert(#bytes > 0, "golden fixture must not be empty")
    -- Fixed prefix from WalletSnapshot default config (see wallet_snapshot.rs
    -- tests): the first bytes are the ERC20 portal address
    -- (rollups-contracts v3.0.0-alpha.6 deterministic deployment, 0x22E5…).
    assert_eq(bytes:byte(1), 0x22)
    assert_eq(bytes:byte(2), 0xe5)
end)

test("shared partition vector matches l1_reader bisect plan", function()
    local vector = load_partition_vector()
    local codes = vector.long_block_range_error_codes

    local defaults = l1_reader.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES
    assert_eq(#codes, #defaults)
    for i, code in ipairs(codes) do
        assert_eq(code, defaults[i])
    end

    for _, scenario in ipairs(vector.scenarios) do
        local calls = {}
        local fails = fail_lookup(scenario.fail_ranges)
        local rpc = {}
        function rpc.get_logs(_self, filter)
            table.insert(calls, { filter.from_block, filter.to_block })
            local message = fails[filter.from_block .. ":" .. filter.to_block]
            if message then
                return nil, message
            end
            return {}
        end

        local logs, err = l1_reader.fetch_logs_partitioned(rpc, {
            start_block = scenario.start_block,
            end_block = scenario.end_block,
            input_box_address = "0xinputbox",
            app_address = "0x1111111111111111111111111111111111111111",
            long_block_range_error_codes = codes,
        })

        if scenario.expect_ok then
            assert(logs, scenario.name .. ": " .. tostring(err))
        else
            assert_eq(logs, nil, scenario.name)
            assert(type(err) == "string", scenario.name)
        end

        assert_eq(#calls, #scenario.expect_calls, scenario.name .. " call count")
        for i, expected in ipairs(scenario.expect_calls) do
            assert_eq(calls[i][1], expected[1], scenario.name .. " call " .. i .. " from")
            assert_eq(calls[i][2], expected[2], scenario.name .. " call " .. i .. " to")
        end
    end
end)

test("l1_reader streams successful partitions in L1 order", function()
    local calls = {}
    local chunks = {}
    local rpc = {}
    function rpc.get_logs(_self, filter)
        table.insert(calls, { filter.from_block, filter.to_block })
        if filter.from_block == 1 and filter.to_block == 4 then
            return nil, "-32005: range too large"
        end
        if filter.from_block == 1 and filter.to_block == 2 then
            return {
                { blockNumber = "0x2", transactionIndex = "0x0", logIndex = "0x2" },
                { blockNumber = "0x1", transactionIndex = "0x0", logIndex = "0x1" },
            }
        end
        if filter.from_block == 3 and filter.to_block == 4 then
            return {
                { blockNumber = "0x4", transactionIndex = "0x0", logIndex = "0x0" },
            }
        end
        error("unexpected range")
    end

    local count, err = l1_reader.for_each_log_chunk_partitioned(rpc, {
        start_block = 1,
        end_block = 4,
        input_box_address = "0xinputbox",
        app_address = "0x1111111111111111111111111111111111111111",
        long_block_range_error_codes = { "-32005" },
    }, function(logs, range)
        table.insert(chunks, {
            from_block = range.from_block,
            to_block = range.to_block,
            first_block = logs[1] and logs[1].blockNumber,
            count = #logs,
        })
        return true
    end)

    assert(count, err)
    assert_eq(count, 3)
    assert_eq(#calls, 3)
    assert_eq(calls[1][1], 1)
    assert_eq(calls[1][2], 4)
    assert_eq(calls[2][1], 1)
    assert_eq(calls[2][2], 2)
    assert_eq(calls[3][1], 3)
    assert_eq(calls[3][2], 4)
    assert_eq(#chunks, 2)
    assert_eq(chunks[1].from_block, 1)
    assert_eq(chunks[1].to_block, 2)
    assert_eq(chunks[1].first_block, "0x1")
    assert_eq(chunks[2].from_block, 3)
    assert_eq(chunks[2].to_block, 4)
end)

test("l1_reader ensure_rpc_head_at_least accepts head at target", function()
    local head, err = l1_reader.ensure_rpc_head_at_least({
        get_block_number_by_tag = function(_self, tag)
            assert_eq(tag, "latest")
            return 42
        end,
    }, 42)
    assert_eq(head, 42)
    assert_eq(err, nil)
end)

test("l1_reader ensure_rpc_head_at_least rejects lagging head", function()
    local head, err = l1_reader.ensure_rpc_head_at_least({
        get_block_number_by_tag = function()
            return 8
        end,
    }, 10)
    assert_eq(head, nil)
    assert(type(err) == "string", "expected retry error")
    assert(err:find("lags target block", 1, true) ~= nil, err)
end)

test("shared log sort vector matches l1_reader.sort_logs", function()
    local vector = load_partition_vector()
    local logs = vector.log_sort.unsorted
    l1_reader.sort_logs(logs)

    for i, expected in ipairs(vector.log_sort.expect_block_order) do
        assert_eq(logs[i].blockNumber, expected, "block order at " .. i)
    end
    for i, expected in ipairs(vector.log_sort.expect_log_index_order) do
        assert_eq(logs[i].logIndex, expected, "log index order at " .. i)
    end
end)

test("jsonrpc get_logs builds InputAdded app filter", function()
    local captured = nil
    local json = {}
    function json.encode(value)
        captured = value
        return "encoded"
    end
    function json.decode(_body)
        return { jsonrpc = "2.0", id = 1, result = {} }
    end

    local http = {}
    function http.post(_self, url, body, headers)
        assert_eq(url, "http://rpc")
        assert_eq(body, "encoded")
        assert_eq(headers["content-type"], "application/json")
        return { status = 200, body = "{}" }
    end

    local client = jsonrpc.new(http, json, "http://rpc")
    local logs, err = client:get_logs({
        address = "0x9999999999999999999999999999999999999999",
        app_address = "0x1111111111111111111111111111111111111111",
        from_block = 10,
        to_block = 12,
        input_added_topic = l1_reader.INPUT_ADDED_TOPIC,
    })

    assert(logs, err)
    assert(type(captured) == "table", "json request captured")
    local request = captured
    assert_eq(request.method, "eth_getLogs")
    local filter = request.params[1]
    assert_eq(filter.fromBlock, "0xa")
    assert_eq(filter.toBlock, "0xc")
    assert_eq(filter.address, "0x9999999999999999999999999999999999999999")
    assert_eq(filter.topics[1], l1_reader.INPUT_ADDED_TOPIC)
    assert_eq(
        filter.topics[2],
        "0x0000000000000000000000001111111111111111111111111111111111111111"
    )
end)

test("config loads snapshot directory safe block and optional topic", function()
    local env = {
        CARTESI_WATCHDOG_SEQUENCER_URL = "http://seq",
        CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS = "0x9999999999999999999999999999999999999999",
        CARTESI_WATCHDOG_APP_ADDRESS = "0x1111111111111111111111111111111111111111",
        CARTESI_WATCHDOG_INPUT_ADDED_TOPIC = "0xtopic",
        CARTESI_WATCHDOG_STATE_DIR = "/tmp/watchdog-state",
        CARTESI_WATCHDOG_CM_SNAPSHOT_DIR = "/tmp/snapshot",
        CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK = "42",
    }

    local cfg = config.load(env)

    assert_eq(cfg.input_added_topic, "0xtopic")
    assert_eq(cfg.state_dir, "/tmp/watchdog-state")
    assert_eq(cfg.cm_snapshot_dir, "/tmp/snapshot")
    assert_eq(cfg.cm_snapshot_safe_block, 42)
    assert_eq(cfg.l1_rpc_url, nil)
end)

test("config requires a sequencer URL", function()
    local ok, err = pcall(function()
        config.load({
            CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS = "0x9999999999999999999999999999999999999999",
            CARTESI_WATCHDOG_APP_ADDRESS = "0x1111111111111111111111111111111111111111",
            CARTESI_WATCHDOG_STATE_DIR = "/tmp/watchdog-state",
        })
    end)
    assert_eq(ok, false)
    assert(tostring(err):find("CARTESI_WATCHDOG_SEQUENCER_URL", 1, true) ~= nil, "sequencer URL is required")
end)

test("checkpoint writes manifest-backed head pointer", function()
    local dir = os.tmpname()
    os.remove(dir)
    os.execute(string.format('mkdir -p "%s"', dir))

    local written, err = checkpoint.write(dir, 12, function(snapshot_dir)
        os.execute(string.format('mkdir -p "%s"', snapshot_dir))
        local file = io.open(snapshot_dir .. "/marker", "wb")
        assert(file ~= nil, "marker file opened")
        file:write("snapshot")
        file:close()
        return true
    end, {
        created_at = "2026-04-28T00:00:00Z",
    })
    assert(written, err)

    local loaded, load_err = checkpoint.load(dir)
    assert(loaded, load_err)
    assert_eq(loaded.snapshot_dir, dir .. "/checkpoints/00000000000000000012/snapshot")
    assert(loaded.manifest_json:find('"safe_block":12', 1, true) ~= nil, "manifest has safe block")
end)

test("checkpoint load rejects missing head pointer", function()
    local dir = os.tmpname()
    os.remove(dir)
    os.execute(string.format('mkdir -p "%s"', dir))

    local loaded, err = checkpoint.load(dir)
    assert_eq(loaded, nil)
    assert_eq(err, "missing head.json")
end)

test("checkpoint rejects head pointer outside checkpoint namespace", function()
    local dir = os.tmpname()
    os.remove(dir)
    os.execute(string.format('mkdir -p "%s"', dir))
    local file = assert(io.open(dir .. "/head.json", "wb"), "head pointer opened")
    file:write('{"checkpoint":"../outside"}\n')
    file:close()

    local loaded, err = checkpoint.load(dir)
    assert_eq(loaded, nil)
    assert_eq(err, "invalid checkpoint pointer")
end)

test("checkpoint rejects manifest without safe block", function()
    local safe_block, err = checkpoint.safe_block_from_manifest("{}")
    assert_eq(safe_block, nil)
    assert_eq(err, "manifest missing safe_block")
end)

test("checkpoint prepare clears stale snapshot dir before write", function()
    local dir = os.tmpname()
    os.remove(dir)
    local stale_snapshot = dir .. "/checkpoints/00000000000000000012/snapshot"
    os.execute(string.format('mkdir -p "%s"', stale_snapshot))
    local stale = io.open(stale_snapshot .. "/garbage", "wb")
    assert(stale ~= nil, "stale file opened")
    stale:write("leftover")
    stale:close()

    local written, err = checkpoint.write(dir, 12, function(snapshot_dir)
        os.execute(string.format('mkdir -p "%s"', snapshot_dir))
        local file = io.open(snapshot_dir .. "/marker", "wb")
        assert(file ~= nil, "marker file opened")
        file:write("fresh")
        file:close()
        return true
    end)
    assert(written, err)

    local marker = io.open(stale_snapshot .. "/marker", "rb")
    assert(marker ~= nil, "fresh marker exists")
    assert_eq(marker:read("*a"), "fresh")
    marker:close()
    assert_eq(io.open(stale_snapshot .. "/garbage", "rb"), nil)
end)

test("checkpoint refuses same-block rewrite before clearing selected snapshot", function()
    local dir = os.tmpname()
    os.remove(dir)

    local written, err = checkpoint.write(dir, 12, function(snapshot_dir)
        os.execute(string.format('mkdir -p "%s"', snapshot_dir))
        local file = io.open(snapshot_dir .. "/marker", "wb")
        assert(file ~= nil, "marker file opened")
        file:write("original")
        file:close()
        return true
    end)
    assert(written, err)

    local second, second_err = checkpoint.write(dir, 12, function(_snapshot_dir)
        error("same-block rewrite must fail before snapshot_writer")
    end)
    assert_eq(second, nil)
    assert(tostring(second_err):find("refusing to rewrite selected checkpoint", 1, true) ~= nil, tostring(second_err))

    local marker = io.open(dir .. "/checkpoints/00000000000000000012/snapshot/marker", "rb")
    assert(marker ~= nil, "selected checkpoint snapshot remains intact")
    assert_eq(marker:read("*a"), "original")
    marker:close()
end)

test("checkpoint write keeps only the current checkpoint (prunes predecessor)", function()
    local dir = os.tmpname()
    os.remove(dir)
    os.execute(string.format('mkdir -p "%s"', dir))

    -- A non-checkpoint sentinel must never be touched by GC: head.json never
    -- points at it.
    local sentinel = dir .. "/genesis-image"
    os.execute(string.format('mkdir -p "%s"', sentinel))

    local function write_block(safe_block)
        local written, err = checkpoint.write(dir, safe_block, function(snapshot_dir)
            os.execute(string.format('mkdir -p "%s"', snapshot_dir))
            local file = assert(io.open(snapshot_dir .. "/marker", "wb"), "marker opened")
            file:write("snapshot")
            file:close()
            return true
        end)
        assert(written, err)
    end

    local function dir_exists(path)
        local ok, _, code = os.rename(path, path)
        return ok or code == 13
    end

    write_block(1)
    write_block(2)
    write_block(3)

    -- Only the latest checkpoint survives; the two predecessors are reclaimed.
    assert(dir_exists(dir .. "/checkpoints/00000000000000000003"), "current checkpoint kept")
    assert_eq(dir_exists(dir .. "/checkpoints/00000000000000000002"), false)
    assert_eq(dir_exists(dir .. "/checkpoints/00000000000000000001"), false)

    -- head.json still resolves to the latest, and GC never touched the sentinel.
    local loaded, load_err = checkpoint.load(dir)
    assert(loaded, load_err)
    assert_eq(loaded.safe_block, 3)
    assert(dir_exists(sentinel), "non-checkpoint dir must be untouched")
end)

local function fake_cfg()
    return {
        state_dir = "/tmp/watchdog-test",
        sequencer_url = "http://sequencer",
        l1_rpc_url = "http://rpc",
        cm_snapshot_dir = "/tmp/genesis-snapshot",
        cm_snapshot_safe_block = 0,
        input_box_address = "0xinputbox",
        app_address = "0x1111111111111111111111111111111111111111",
        blockchain_id = "31337",
        input_added_topic = "0xtopic",
        long_block_range_error_codes = l1_reader.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES,
        retry_attempts = 1,
        retry_delay_sec = 0,
    }
end

local function fake_machine(inspect_state)
    local machine = {
        loaded_path = nil,
        fed_inputs = nil,
        advance_calls = {},
    }
    function machine:load(path, reference_block)
        self.loaded_path = path
        return { path = path, reference_block = reference_block or 0 }
    end
    function machine:advance(_instance, inputs, range)
        self.fed_inputs = inputs
        table.insert(self.advance_calls, {
            from_block = range.from_block,
            to_block = range.to_block,
            input_count = #inputs,
        })
        _instance.reference_block = range.to_block
        return true
    end
    function machine:inspect(_self, _instance)
        return inspect_state
    end
    function machine:dump(_instance, snapshot_dir, reference_block)
        self.saved_snapshot_dir = snapshot_dir
        _instance.reference_block = reference_block
        -- Real CM dumps create a non-empty snapshot dir; mirror that so
        -- idempotent-init usability checks see a complete state.
        os.execute("mkdir -p " .. "'" .. tostring(snapshot_dir):gsub("'", "'\\''") .. "'")
        local marker = io.open(snapshot_dir .. "/.dump", "w")
        assert(marker, "write snapshot marker")
        marker:write("ok")
        marker:close()
        return true
    end
    function machine:feed_inputs(instance, inputs)
        local from = (instance.reference_block or 0) + 1
        return self:advance(instance, inputs, { from_block = from, to_block = from })
    end
    function machine:inspect_state(_self, instance)
        return self:inspect(_self, instance)
    end
    function machine:save(instance, snapshot_dir)
        return self:dump(instance, snapshot_dir, instance.reference_block)
    end
    return machine
end

test("tick config prefers blockchain id env over persisted config", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir
    cfg.blockchain_id = nil

    local result, err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(result, err)

    local tick_cfg = main_mod.load_tick_config({
        CARTESI_WATCHDOG_STATE_DIR = dir,
        CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT = "http://tick-rpc",
        CARTESI_WATCHDOG_BLOCKCHAIN_ID = "31337",
    })
    assert_eq(tick_cfg.blockchain_id, "31337")
end)

test("metrics queries chain id from eth_chainId when unset", function()
    local chain_id = metrics.query_chain_id_from_rpc("http://l1-rpc", function(url)
        assert_eq(url, "http://l1-rpc")
        return {
            get_chain_id = function()
                return 31337
            end,
        }
    end)
    assert_eq(chain_id, "31337")
end)

test("init persists blockchain id from eth_chainId when env unset", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir
    cfg.blockchain_id = nil

    local result, err = main_mod.run_init(cfg, {
        machine = fake_machine("{}"),
        env = function(name)
            if name == "CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT" then
                return "http://init-rpc"
            end
            return nil
        end,
        rpc_factory = function(url)
            assert_eq(url, "http://init-rpc")
            return {
                get_chain_id = function()
                    return 31337
                end,
            }
        end,
    })
    assert(result, err)

    local tick_cfg = main_mod.load_tick_config({
        CARTESI_WATCHDOG_STATE_DIR = dir,
        CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT = "http://tick-rpc",
    })
    assert_eq(tick_cfg.blockchain_id, "31337")
end)

test("http request errors include method and url", function()
    local http_mod = require("watchdog.http")
    local message = http_mod.format_request_error(
        "GET",
        "http://127.0.0.1:54321/finalized_state/inclusion_block",
        "[CURL-EASY][COULDNT_CONNECT] Could not connect to server (7)"
    )
    assert(
        message:find("GET http://127.0.0.1:54321/finalized_state/inclusion_block failed:", 1, true) ~= nil,
        message
    )
end)

test("jsonrpc reads eth_chainId", function()
    local http = {}
    function http.post(_self, url, body, _headers)
        assert_eq(url, "http://l1-rpc")
        assert(body:find('"eth_chainId"', 1, true) ~= nil, body)
        return {
            status = 200,
            body = '{"jsonrpc":"2.0","id":1,"result":"0x7a69"}',
            headers = {},
        }
    end
    local json = require("watchdog.json").new()
    local client = jsonrpc.new(http, json, "http://l1-rpc")
    local chain, err = client:get_chain_id()
    assert(chain, err)
    assert_eq(chain, 31337)
end)

local function load_fixture(path)
    local file, err = io.open(path, "rb")
    if not file then
        error("open " .. path .. ": " .. tostring(err))
    end
    local body = file:read("*a")
    file:close()
    return body
end

test("main tick writes status.prom through exit path", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir
    local init_result, init_err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(init_result, init_err)

    local tick_env = {
        CARTESI_WATCHDOG_STATE_DIR = dir,
        CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT = "http://tick-rpc",
    }
    local tick_deps = {
        checkpoint = {
            load = function(_state_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 5 }
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 5, l2_tx_index = 0 }
            end,
        },
        machine = fake_machine("{}"),
    }

    local exit_code, run_err = capture_os_exit(function()
        main_mod.main({ "tick" }, { deps = tick_deps, env = tick_env })
    end)
    assert(run_err == nil, tostring(run_err))
    assert_eq(exit_code, main_mod.EXIT_OK)

    local file = assert(io.open(dir .. "/status.prom", "rb"))
    local body = file:read("*a")
    file:close()
    assert(body:find('state="ok"} 1', 1, true) ~= nil, body)
    assert(body:find('cartesi_watchdog_status{app_address="0x1111111111111111111111111111111111111111",chain="31337",state="ok"} 1', 1, true) ~= nil, body)
    assert(body:find("cartesi_watchdog_exit_code", 1, true) == nil, body)
    assert(body:find("cartesi_watchdog_last_tick_unix_seconds", 1, true) == nil, body)
end)

test("metrics prom matches golden ok fixture", function()
    local body = metrics.build_prom({
        exit_code = 0,
        chain_id = "11155111",
        app_address = "0x4CE633CA71071818cD73187765ee60F696dae083",
    })
    assert_eq(body, load_fixture("tests/fixtures/watchdog_status_ok.prom"))
end)

test("metrics prom matches golden failed fixture", function()
    local body = metrics.build_prom({
        exit_code = 2,
        chain_id = "31337",
        app_address = "0xdeadbeef",
        divergence_kind = "state_mismatch",
    })
    assert_eq(body, load_fixture("tests/fixtures/watchdog_status_failed.prom"))
end)

test("metrics prom marks warning state without divergence info", function()
    local body = metrics.build_prom({
        exit_code = 1,
        chain_id = "31337",
        app_address = "0xdeadbeef",
    })

    assert(body:find('state="warning"} 1', 1, true) ~= nil, body)
    assert(body:find("cartesi_watchdog_divergence_info", 1, true) == nil, body)
    assert(body:find("cartesi_watchdog_exit_code", 1, true) == nil, body)
end)

test("metrics resolve_path honors custom metrics file env", function()
    local path = metrics.resolve_path({ state_dir = "/var/lib/watchdog" }, {
        CARTESI_WATCHDOG_METRICS_FILE = "/tmp/custom.prom",
    })
    assert_eq(path, "/tmp/custom.prom")
end)

test("metrics resolve_path defaults to state dir status prom", function()
    local path = metrics.resolve_path({ state_dir = "/var/lib/watchdog" }, {})
    assert_eq(path, "/var/lib/watchdog/status.prom")
end)

test("metrics resolve_path returns error when state dir env missing", function()
    local path, err = metrics.resolve_path({}, {})
    assert_eq(path, nil)
    assert(err:find("CARTESI_WATCHDOG_STATE_DIR is required", 1, true) ~= nil, err)
end)

test("metrics prom uses unknown labels when chain and app are missing", function()
    local body = metrics.build_prom({
        exit_code = 0,
    })
    assert(body:find('chain="unknown"', 1, true) ~= nil, body)
    assert(body:find('app_address="unknown"', 1, true) ~= nil, body)
end)

test("metrics prom escapes label special characters", function()
    local body = metrics.build_prom({
        exit_code = 2,
        chain_id = '31"337',
        app_address = '0x\\addr',
        divergence_kind = "state_mismatch",
    })
    assert(body:find('chain="31\\"337"', 1, true) ~= nil, body)
    assert(body:find('app_address="0x\\\\addr"', 1, true) ~= nil, body)
end)

test("metrics write is atomic and leaves no tmp file", function()
    local dir = os.tmpname()
    os.remove(dir)

    local prom_path = dir .. "/status.prom"
    local ok, err = metrics.write_tick_status({
        cfg = { state_dir = dir, app_address = "0xabc", blockchain_id = "1" },
        exit_code = 0,
    })
    assert(ok, err)

    local tmp = io.open(prom_path .. ".tmp", "rb")
    assert_eq(tmp, nil)
    local file = assert(io.open(prom_path, "rb"))
    local body = file:read("*a")
    file:close()
    assert(body:find('state="ok"} 1', 1, true) ~= nil, body)
end)

test("successful idle compare writes ok status prom", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir

    local result, err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(result, err)

    local exit_code, payload = main_mod.run_compare_cycle(cfg, {
        checkpoint = {
            load = function(_state_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 5 }
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 5, l2_tx_index = 0 }
            end,
        },
        machine = fake_machine("{}"),
    })
    assert_eq(exit_code, main_mod.EXIT_OK)
    assert(payload.skipped, "expected idle skip")

    main_mod.write_tick_metrics(cfg, exit_code, payload, {
        CARTESI_WATCHDOG_STATE_DIR = dir,
    })

    local file = assert(io.open(dir .. "/status.prom", "rb"))
    local body = file:read("*a")
    file:close()
    assert(body:find('state="ok"} 1', 1, true) ~= nil, body)
    assert(body:find("cartesi_watchdog_divergence_info", 1, true) == nil, body)
end)

test("metrics maps exit codes to status states", function()
    assert_eq(metrics.state_for_exit_code(0), "ok")
    assert_eq(metrics.state_for_exit_code(1), "warning")
    assert_eq(metrics.state_for_exit_code(2), "failed")
end)

test("metrics prom file marks failed state on divergence", function()
    local body = metrics.build_prom({
        exit_code = 2,
        chain_id = "31337",
        app_address = "0xdeadbeef",
        divergence_kind = "state_mismatch",
    })

    assert(body:find('cartesi_watchdog_status{app_address="0xdeadbeef",chain="31337",state="failed"} 1', 1, true) ~= nil, body)
    assert(body:find('cartesi_watchdog_status{app_address="0xdeadbeef",chain="31337",state="ok"} 0', 1, true) ~= nil, body)
    assert(body:find('cartesi_watchdog_divergence_info{app_address="0xdeadbeef",chain="31337",kind="state_mismatch"} 1', 1, true) ~= nil, body)
    assert(body:find("cartesi_watchdog_exit_code", 1, true) == nil, body)
    assert(body:find("cartesi_watchdog_last_tick_unix_seconds", 1, true) == nil, body)
end)

test("tick writes status.prom into watchdog state dir", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir
    cfg.blockchain_id = "31337"

    local result, err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(result, err)

    local exit_code, payload = main_mod.run_compare_cycle(cfg, {
        checkpoint = {
            load = function(_state_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 1 }
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 2, l2_tx_index = 0 }
            end,
            get_finalized_state = function()
                return {
                    inclusion_block = 2,
                    l2_tx_index = 0,
                    state = '{"a":1}',
                }
            end,
        },
        fetch_inputs = function(from_block, to_block)
            assert_eq(from_block, 2)
            assert_eq(to_block, 2)
            return {}
        end,
        machine = fake_machine('{ "a": 1 }'),
    })
    assert_eq(exit_code, main_mod.EXIT_DIVERGENCE)
    assert(type(payload) == "table", "expected divergence payload")
    assert_eq(payload.kind, "state_mismatch")

    main_mod.write_tick_metrics(cfg, exit_code, payload, {
        CARTESI_WATCHDOG_STATE_DIR = dir,
    })

    local prom_path = dir .. "/status.prom"
    local file, open_err = io.open(prom_path, "rb")
    assert(file, open_err)
    local body = file:read("*a")
    file:close()

    assert(body:find('state="failed"} 1', 1, true) ~= nil, body)
    assert(body:find('kind="state_mismatch"} 1', 1, true) ~= nil, body)
end)

test("init stores bootstrap snapshot as watchdog head", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir
    cfg.cm_snapshot_safe_block = 5

    local machine = fake_machine("{}")
    local result, err = main_mod.run_init(cfg, {
        machine = machine,
    })
    assert(result, err)
    assert_eq(result.safe_block, 5)
    assert_eq(machine.loaded_path, "/tmp/genesis-snapshot")

    local loaded, load_err = checkpoint.load(dir)
    assert(loaded, load_err)
    assert_eq(loaded.safe_block, 5)
    assert_eq(loaded.snapshot_dir, dir .. "/checkpoints/00000000000000000005/snapshot")

    local persisted, cfg_err = state_mod.read_json(dir, "config.json", require("watchdog.json").new())
    assert(persisted, cfg_err)
    assert_eq(persisted.sequencer_url, "http://sequencer")
    assert_eq(persisted.l1_rpc_url, nil)
    assert_eq(persisted.blockchain_id, "31337")

    local tick_cfg = main_mod.load_tick_config({
        CARTESI_WATCHDOG_STATE_DIR = dir,
        CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT = "http://tick-rpc",
    })
    assert_eq(tick_cfg.state_dir, dir)
    assert_eq(tick_cfg.sequencer_url, "http://sequencer")
    assert_eq(tick_cfg.l1_rpc_url, "http://tick-rpc")
    assert_eq(tick_cfg.blockchain_id, "31337")
    assert_eq(tick_cfg.app_address, "0x1111111111111111111111111111111111111111")
end)

test("tick config prefers sequencer url env over persisted config", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir

    local result, err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(result, err)

    local tick_cfg = main_mod.load_tick_config({
        CARTESI_WATCHDOG_STATE_DIR = dir,
        CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT = "http://tick-rpc",
        CARTESI_WATCHDOG_SEQUENCER_URL = "http://new-sequencer:9999",
    })
    assert_eq(tick_cfg.sequencer_url, "http://new-sequencer:9999")
end)

test("tick config requires current RPC URL outside persisted state", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir

    local result, err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(result, err)

    local ok, load_err = pcall(function()
        main_mod.load_tick_config({
            CARTESI_WATCHDOG_STATE_DIR = dir,
        })
    end)
    assert_eq(ok, false)
    assert(tostring(load_err):find("CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT", 1, true) ~= nil, tostring(load_err))
end)

test("init is a no-op success when state is already initialized", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir

    local first, first_err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(first, first_err)
    assert_eq(first.already_initialized, nil)

    local second, second_err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(second, second_err)
    assert_eq(second.ok, true)
    assert_eq(second.already_initialized, true)
    assert_eq(second.safe_block, first.safe_block)
end)

test("init fails when config.json is missing after head exists", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir
    local first, first_err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(first, first_err)

    assert(os.remove(dir .. "/config.json"))

    local second, second_err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert_eq(second, nil)
    assert(tostring(second_err):find("config.json", 1, true), tostring(second_err))
    assert(tostring(second_err):find("wipe state_dir", 1, true), tostring(second_err))
end)

test("init fails when checkpoint snapshot is missing after head exists", function()
    local dir = os.tmpname()
    os.remove(dir)

    local cfg = fake_cfg()
    cfg.state_dir = dir
    local first, first_err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert(first, first_err)

    local loaded = assert(checkpoint.load(dir))
    assert(os.execute("rm -rf '" .. loaded.snapshot_dir:gsub("'", "'\\''") .. "'"))

    local second, second_err = main_mod.run_init(cfg, { machine = fake_machine("{}") })
    assert_eq(second, nil)
    assert(tostring(second_err):find("snapshot", 1, true), tostring(second_err))
    assert(tostring(second_err):find("wipe state_dir", 1, true), tostring(second_err))
end)

test("runner happy path replays inputs and writes checkpoint", function()
    local checkpoint_writes = {}
    local checkpoint_mod = {
        load = function(_dir)
            return {
                snapshot_dir = "/tmp/checkpoints/0001/snapshot",
                safe_block = 10,
            }
        end,
        write = function(dir, safe_block, snapshot_writer, manifest)
            local ok, err = snapshot_writer("/tmp/new-snapshot")
            assert(ok, err)
            table.insert(checkpoint_writes, {
                dir = dir,
                safe_block = safe_block,
                manifest = manifest,
            })
            return true
        end,
    }
    local machine = fake_machine('{"ok":true}')
    local result, err = runner.run_once(fake_cfg(), {
        checkpoint = checkpoint_mod,
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 12, l2_tx_index = 0 }
            end,
            get_finalized_state = function()
                return {
                    inclusion_block = 12,
                    l2_tx_index = 0,
                    state = '{"ok":true}',
                }
            end,
        },
        fetch_inputs = function(from_block, to_block)
            assert_eq(from_block, 11)
            assert_eq(to_block, 12)
            return { { payload = "a" }, { payload = "b" } }
        end,
        machine = machine,
    })

    assert(result, err)
    assert_eq(result.safe_block, 12)
    assert_eq(result.input_count, 2)
    assert_eq(machine.loaded_path, "/tmp/checkpoints/0001/snapshot")
    assert_eq(machine.saved_snapshot_dir, "/tmp/new-snapshot")
    assert_eq(#machine.fed_inputs, 2)
    assert_eq(#checkpoint_writes, 1)
    assert_eq(checkpoint_writes[1].safe_block, 12)
end)

test("runner advances CM as streamed input chunks arrive", function()
    local checkpoint_writes = {}
    local checkpoint_mod = {
        load = function(_dir)
            return {
                snapshot_dir = "/tmp/checkpoints/0001/snapshot",
                safe_block = 10,
            }
        end,
        write = function(_dir, safe_block, snapshot_writer, _manifest)
            local ok, err = snapshot_writer("/tmp/new-snapshot")
            assert(ok, err)
            table.insert(checkpoint_writes, safe_block)
            return true
        end,
    }
    local machine = fake_machine('{"ok":true}')
    local result, err = runner.run_once(fake_cfg(), {
        checkpoint = checkpoint_mod,
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 12, l2_tx_index = 0 }
            end,
            get_finalized_state = function()
                return {
                    inclusion_block = 12,
                    l2_tx_index = 0,
                    state = '{"ok":true}',
                }
            end,
        },
        for_each_input_chunk = function(from_block, to_block, on_chunk)
            assert_eq(from_block, 11)
            assert_eq(to_block, 12)
            local ok, chunk_err = on_chunk({ { raw_input = "a" } }, {
                from_block = 11,
                to_block = 11,
            })
            assert(ok, chunk_err)
            ok, chunk_err = on_chunk({ { raw_input = "b" }, { raw_input = "c" } }, {
                from_block = 12,
                to_block = 12,
            })
            assert(ok, chunk_err)
            return 3
        end,
        machine = machine,
    })

    assert(result, err)
    assert_eq(result.input_count, 3)
    assert_eq(#machine.advance_calls, 2)
    assert_eq(machine.advance_calls[1].from_block, 11)
    assert_eq(machine.advance_calls[1].input_count, 1)
    assert_eq(machine.advance_calls[2].from_block, 12)
    assert_eq(machine.advance_calls[2].input_count, 2)
    assert_eq(#checkpoint_writes, 1)
    assert_eq(checkpoint_writes[1], 12)
end)

test("runner advances CM over empty streamed partitions", function()
    local machine = fake_machine('{"ok":true}')
    local result, err = runner.run_once(fake_cfg(), {
        checkpoint = {
            load = function(_dir)
                return {
                    snapshot_dir = "/tmp/checkpoints/0001/snapshot",
                    safe_block = 10,
                }
            end,
            write = function(_dir, safe_block, snapshot_writer, _manifest)
                local ok, write_err = snapshot_writer("/tmp/new-snapshot")
                assert(ok, write_err)
                assert_eq(safe_block, 12)
                return true
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 12, l2_tx_index = 0 }
            end,
            get_finalized_state = function()
                return {
                    inclusion_block = 12,
                    l2_tx_index = 0,
                    state = '{"ok":true}',
                }
            end,
        },
        for_each_input_chunk = function(_from_block, _to_block, on_chunk)
            local ok, chunk_err = on_chunk({}, {
                from_block = 11,
                to_block = 11,
            })
            assert(ok, chunk_err)
            ok, chunk_err = on_chunk({ { raw_input = "a" } }, {
                from_block = 12,
                to_block = 12,
            })
            assert(ok, chunk_err)
            return 1
        end,
        machine = machine,
    })

    assert(result, err)
    assert_eq(result.input_count, 1)
    assert_eq(#machine.advance_calls, 2)
    assert_eq(machine.advance_calls[1].from_block, 11)
    assert_eq(machine.advance_calls[1].to_block, 11)
    assert_eq(machine.advance_calls[1].input_count, 0)
    assert_eq(machine.advance_calls[2].from_block, 12)
    assert_eq(machine.advance_calls[2].input_count, 1)
end)

test("runner returns state mismatch payload", function()
    local result, err = runner.run_once(fake_cfg(), {
        checkpoint = {
            load = function(_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 1 }
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 2, l2_tx_index = 0 }
            end,
            get_finalized_state = function()
                return {
                    inclusion_block = 2,
                    l2_tx_index = 0,
                    state = '{"a":1}',
                }
            end,
        },
        fetch_inputs = function(from_block, to_block)
            assert_eq(from_block, 2)
            assert_eq(to_block, 2)
            return {}
        end,
        machine = fake_machine('{ "a": 1 }'),
    })

    assert_eq(result, nil)
    assert(type(err) == "table", "expected mismatch payload")
    assert_eq(err.kind, "state_mismatch")
end)

test("runner refuses missing or corrupt watchdog head", function()
    local ok, err = pcall(function()
        return runner.run_once(fake_cfg(), {
            checkpoint = {
                load = function(_dir)
                    return nil, "invalid checkpoint pointer"
                end,
            },
            sequencer = {
                get_finalized_inclusion_block = function()
                    error("sequencer must not be queried after corrupt checkpoint")
                end,
            },
            machine = fake_machine("{}"),
        })
    end)

    assert_eq(ok, false)
    assert(tostring(err):find("failed to load watchdog head", 1, true) ~= nil, tostring(err))
    assert(tostring(err):find("sequencer-watchdog init", 1, true) ~= nil, tostring(err))
end)

test("compare cycle does not retry missing watchdog head", function()
    local loads = 0
    local cfg = fake_cfg()
    cfg.retry_attempts = 3
    cfg.retry_delay_sec = 1

    local exit_code, err = main_mod.run_compare_cycle(cfg, {
        checkpoint = {
            load = function(_dir)
                loads = loads + 1
                return nil, "missing head.json"
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                error("sequencer must not be queried after missing head")
            end,
        },
        machine = fake_machine("{}"),
        log_step = function() end,
    })

    assert_eq(exit_code, main_mod.EXIT_TRANSIENT)
    assert_eq(loads, 1)
    assert(tostring(err):find("failed to load watchdog head", 1, true) ~= nil, tostring(err))
    assert(tostring(err):find("missing head.json", 1, true) ~= nil, tostring(err))
end)

test("tick missing head writes warning status.prom once", function()
    local dir = os.tmpname()
    os.remove(dir)
    assert(state_mod.ensure_dir(dir))

    local json = require("watchdog.json").new()
    local ok_write, write_err = state_mod.write_json_atomic(dir, "config.json", {
        version = 1,
        sequencer_url = "http://sequencer",
        input_box_address = "0xinputbox",
        app_address = "0x1111111111111111111111111111111111111111",
        blockchain_id = "31337",
        retry_attempts = 3,
        retry_delay_sec = 0,
        long_block_range_error_codes = {},
    }, json)
    assert(ok_write, write_err)

    -- Pre-seed a stale ok file so we can assert the tick rewrites it.
    local seed_ok, seed_err = metrics.write_tick_status({
        cfg = {
            state_dir = dir,
            app_address = "0x1111111111111111111111111111111111111111",
            blockchain_id = "31337",
        },
        exit_code = 0,
    })
    assert(seed_ok, seed_err)

    local loads = 0
    local exit_code, run_err = capture_os_exit(function()
        main_mod.main({ "tick" }, {
            env = {
                CARTESI_WATCHDOG_STATE_DIR = dir,
                CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT = "http://tick-rpc",
            },
            deps = {
                checkpoint = {
                    load = function(state_dir)
                        loads = loads + 1
                        assert_eq(state_dir, dir)
                        return nil, "missing head.json"
                    end,
                },
                sequencer = {
                    get_finalized_inclusion_block = function()
                        error("sequencer must not be queried after missing head")
                    end,
                },
                machine = fake_machine("{}"),
                log_step = function() end,
            },
        })
    end)
    assert(run_err == nil, tostring(run_err))
    assert_eq(exit_code, main_mod.EXIT_TRANSIENT)
    assert_eq(loads, 1)

    local file = assert(io.open(dir .. "/status.prom", "rb"))
    local body = file:read("*a")
    file:close()
    assert(body:find('state="warning"} 1', 1, true) ~= nil, body)
    assert(body:find('state="ok"} 1', 1, true) == nil, body)
end)

test("runner returns transient error when L1 RPC head lags target block", function()
    local result, err = runner.run_once(fake_cfg(), {
        checkpoint = {
            load = function(_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 5 }
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 10, l2_tx_index = 0 }
            end,
        },
        rpc = {
            get_block_number_by_tag = function(_self, tag)
                assert_eq(tag, "latest")
                return 8
            end,
            get_logs = function()
                error("get_logs must not run when RPC head lags target block")
            end,
        },
        machine = fake_machine(string.char(1)),
    })

    assert_eq(result, nil)
    assert(type(err) == "string", "expected transient retry error")
    assert(err:find("RPC latest head", 1, true) ~= nil, err)
end)

test("runner returns transient error when finalized inclusion_block moves during compare", function()
    local result, err = runner.run_once(fake_cfg(), {
        checkpoint = {
            load = function(_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 0 }
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 1, l2_tx_index = 0 }
            end,
            get_finalized_state = function()
                return {
                    inclusion_block = 2,
                    l2_tx_index = 0,
                    state = string.char(1),
                }
            end,
        },
        fetch_inputs = function(from_block, to_block)
            assert_eq(from_block, 1)
            assert_eq(to_block, 1)
            return {}
        end,
        machine = fake_machine(string.char(1)),
    })

    assert_eq(result, nil)
    assert(type(err) == "string", "expected transient retry error")
    assert(err:find("inclusion_block moved", 1, true) ~= nil, err)
end)

test("runner skips compare cycle when finalized inclusion_block is unchanged", function()
    local machine = fake_machine('{"ok":true}')
    local result, err = runner.run_once(fake_cfg(), {
        checkpoint = {
            load = function(_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 5 }
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 5, l2_tx_index = 0 }
            end,
            get_finalized_state = function()
                error("get_finalized_state must not run when inclusion_block is unchanged")
            end,
        },
        fetch_inputs = function()
            error("fetch_inputs must not run when inclusion_block is unchanged")
        end,
        machine = machine,
    })

    assert(result, err)
    assert_eq(result.skipped, true)
    assert_eq(result.skip_reason, "finalized_unchanged")
    assert_eq(result.safe_block, 5)
    assert_eq(machine.fed_inputs, nil)
end)

test("runner returns sequencer inclusion_block regression payload", function()
    local result, err = runner.run_once(fake_cfg(), {
        checkpoint = {
            load = function(_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 5 }
            end,
        },
        sequencer = {
            get_finalized_inclusion_block = function()
                return { inclusion_block = 4, l2_tx_index = 0 }
            end,
            get_finalized_state = function()
                return {
                    inclusion_block = 4,
                    l2_tx_index = 0,
                    state = "{}",
                }
            end,
        },
        machine = fake_machine("{}"),
    })

    assert_eq(result, nil)
    assert(type(err) == "table", "expected regression payload")
    assert_eq(err.kind, "inclusion_block_regressed")
end)

test("sequencer client reads finalized inclusion_block", function()
    local http = {}
    function http.get(_self, url)
        assert_eq(url, "http://sequencer/finalized_state/inclusion_block")
        return {
            status = 200,
            body = '{"inclusion_block":7,"l2_tx_index":3}',
            headers = {},
        }
    end
    local json = {}
    function json.decode(body)
        return {
            inclusion_block = 7,
            l2_tx_index = 3,
        }
    end

    local client = sequencer_reader.new(http, json, "http://sequencer/")
    local head, err = client:get_finalized_inclusion_block()
    assert(head, err)
    assert_eq(head.inclusion_block, 7)
    assert_eq(head.l2_tx_index, 3)
end)

test("sequencer client reads finalized SSZ body and headers", function()
    local http = {}
    function http.get(_self, url, _headers)
        assert_eq(url, "http://sequencer/finalized_state")
        return {
            status = 200,
            body = "raw-state",
            headers = {
                ["x-inclusion-block"] = "9",
                ["x-l2-tx-index"] = "1",
            },
        }
    end
    local json = {}
    function json.decode(_body)
        error("unexpected JSON decode for finalized_state body")
    end

    local client = sequencer_reader.new(http, json, "http://sequencer")
    local state, err = client:get_finalized_state()
    assert(state, err)
    assert_eq(state.inclusion_block, 9)
    assert_eq(state.l2_tx_index, 1)
    assert_eq(state.state, "raw-state")
end)

test("sequencer client rejects invalid inclusion_block JSON", function()
    local http = {}
    function http.get(_self, _url)
        return {
            status = 200,
            body = "not-json",
            headers = {},
        }
    end
    local json = {}
    function json.decode(_body)
        error("decode failed")
    end

    local client = sequencer_reader.new(http, json, "http://sequencer")
    local head, err = client:get_finalized_inclusion_block()
    assert_eq(head, nil)
    assert_eq(err, "invalid finalized inclusion_block response JSON")
end)

test("retry succeeds after transient failures", function()
    local attempts = 0
    local sleeps = 0
    local result, err = retry.with_retries(function()
        attempts = attempts + 1
        if attempts < 3 then
            return nil, "transient"
        end
        return "ok"
    end, {
        attempts = 3,
        delay_sec = 1,
        sleep = function(seconds)
            assert_eq(seconds, 1)
            sleeps = sleeps + 1
        end,
    })

    assert_eq(result, "ok")
    assert_eq(err, nil)
    assert_eq(attempts, 3)
    assert_eq(sleeps, 2)
end)

test("retry returns final error after exhaustion", function()
    local attempts = 0
    local result, err = retry.with_retries(function()
        attempts = attempts + 1
        return nil, "failed-" .. tostring(attempts)
    end, {
        attempts = 2,
        delay_sec = 0,
        sleep = function() end,
    })

    assert_eq(result, nil)
    assert_eq(err, "failed-2")
    assert_eq(attempts, 2)
end)

test("retry stops immediately on terminal errors", function()
    local attempts = 0
    local sleeps = 0
    local result, err = retry.with_retries(function()
        attempts = attempts + 1
        return nil, { kind = "state_mismatch" }
    end, {
        attempts = 3,
        delay_sec = 1,
        should_retry = function(retry_err)
            return not (type(retry_err) == "table" and retry_err.kind == "state_mismatch")
        end,
        sleep = function(_seconds)
            sleeps = sleeps + 1
        end,
    })

    assert_eq(result, nil)
    assert(type(err) == "table", "terminal payload returned")
    assert_eq(err.kind, "state_mismatch")
    assert_eq(attempts, 1)
    assert_eq(sleeps, 0)
end)

local passed = 0
local failed = {}
for _, t in ipairs(tests) do
    local ok, err = pcall(t.fn)
    if ok then
        passed = passed + 1
        io.write("ok - " .. t.name .. "\n")
    else
        table.insert(failed, { name = t.name, err = tostring(err) })
        io.stderr:write("FAIL - " .. t.name .. ": " .. tostring(err) .. "\n")
    end
end

local total = #tests
io.write(string.format("\nwatchdog unit tests: %d/%d passed\n", passed, total))
if #failed > 0 then
    io.stderr:write(string.format(
        "\n*** %d TEST(S) FAILED ***\n",
        #failed
    ))
    for i, entry in ipairs(failed) do
        io.stderr:write(string.format("  %d. %s\n     %s\n", i, entry.name, entry.err))
    end
    io.stderr:write("\n")
    os.exit(1)
end
io.write("all tests passed\n")
