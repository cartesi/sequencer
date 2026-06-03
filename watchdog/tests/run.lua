-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

package.path = "./?.lua;./?/init.lua;" .. package.path

local abi = require("watchdog.abi")
local alarm = require("watchdog.alarm")
local checkpoint = require("watchdog.checkpoint")
local compare = require("watchdog.compare")
local config = require("watchdog.config")
local jsonrpc = require("watchdog.jsonrpc")
local l1_reader = require("watchdog.l1_reader")
local machine_cli = require("watchdog.machine_cli")
local retry = require("watchdog.retry")
local runner = require("watchdog.runner")
local sequencer_reader = require("watchdog.sequencer_reader")

local tests = {}

local function test(name, fn)
    table.insert(tests, { name = name, fn = fn })
end

local function assert_eq(actual, expected)
    if actual ~= expected then
        error(string.format("expected %q, got %q", tostring(expected), tostring(actual)), 2)
    end
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
    local path = "tests/fixtures/wallet_snapshot_v1_empty.hex"
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
    -- Fixed prefix from WalletSnapshotV1 default config (see wallet_snapshot.rs tests).
    assert_eq(bytes:byte(1), 0xac)
    assert_eq(bytes:byte(2), 0xa6)
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
        WATCHDOG_L1_RPC_URL = "http://rpc",
        WATCHDOG_INPUTBOX_ADDRESS = "0x9999999999999999999999999999999999999999",
        WATCHDOG_APP_ADDRESS = "0x1111111111111111111111111111111111111111",
        WATCHDOG_INPUT_ADDED_TOPIC = "0xtopic",
        WATCHDOG_CHECKPOINT_DIR = "/tmp/checkpoints",
        WATCHDOG_CM_SNAPSHOT_DIR = "/tmp/snapshot",
        WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK = "42",
    }

    local cfg = config.load(env)

    assert_eq(cfg.input_added_topic, "0xtopic")
    assert_eq(cfg.cm_snapshot_dir, "/tmp/snapshot")
    assert_eq(cfg.cm_snapshot_safe_block, 42)
    assert_eq(cfg.mode, "advance")
end)

test("config rejects unknown mode", function()
    local ok, err = pcall(function()
        config.load({
            WATCHDOG_MODE = "bad",
            WATCHDOG_L1_RPC_URL = "http://rpc",
            WATCHDOG_INPUTBOX_ADDRESS = "0x9999999999999999999999999999999999999999",
            WATCHDOG_APP_ADDRESS = "0x1111111111111111111111111111111111111111",
            WATCHDOG_CHECKPOINT_DIR = "/tmp/checkpoints",
        })
    end)
    assert_eq(ok, false)
    assert(tostring(err):find("WATCHDOG_MODE", 1, true) ~= nil, "mode error is explicit")
end)

test("checkpoint writes manifest-backed current pointer", function()
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

test("checkpoint rejects manifest without safe block", function()
    local safe_block, err = checkpoint.safe_block_from_manifest("{}")
    assert_eq(safe_block, nil)
    assert_eq(err, "manifest missing safe_block")
end)

local function fake_cfg()
    return {
        checkpoint_dir = "/tmp/watchdog-test",
        cm_snapshot_dir = "/tmp/genesis-snapshot",
        cm_snapshot_safe_block = 0,
        input_box_address = "0xinputbox",
        app_address = "0x1111111111111111111111111111111111111111",
        input_added_topic = "0xtopic",
        long_block_range_error_codes = l1_reader.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES,
    }
end

local function fake_machine(inspect_state)
    local machine = {
        loaded_path = nil,
        fed_inputs = nil,
    }
    function machine:load(path, reference_block)
        self.loaded_path = path
        return { path = path, reference_block = reference_block or 0 }
    end
    function machine:advance(_instance, inputs, range)
        self.fed_inputs = inputs
        _instance.reference_block = range.to_block
        return true
    end
    function machine:inspect(_self, _instance)
        return inspect_state
    end
    function machine:dump(_instance, snapshot_dir, reference_block)
        self.saved_snapshot_dir = snapshot_dir
        _instance.reference_block = reference_block
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

test("runner alarms on raw state mismatch", function()
    local alarms = {}
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
        alarm = function(payload)
            table.insert(alarms, payload)
            return true
        end,
    })

    assert_eq(result, nil)
    assert(type(err) == "table", "expected mismatch payload")
    assert_eq(err.kind, "state_mismatch")
    assert_eq(#alarms, 1)
    assert_eq(alarms[1].kind, "state_mismatch")
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

test("runner alarms on sequencer inclusion_block regression", function()
    local alarms = {}
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
        alarm = function(payload)
            table.insert(alarms, payload)
            return true
        end,
    })

    assert_eq(result, nil)
    assert(type(err) == "table", "expected regression payload")
    assert_eq(err.kind, "inclusion_block_regressed")
    assert_eq(#alarms, 1)
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

test("advance runner fetches inputs and saves checkpoint without sequencer", function()
    local checkpoint_writes = {}
    local machine = fake_machine("unused")
    local result, err = runner.advance_checkpoint_once(fake_cfg(), {
        checkpoint = {
            load = function(_dir)
                return { snapshot_dir = "/tmp/snapshot", safe_block = 7 }
            end,
            write = function(dir, safe_block, snapshot_writer, manifest)
                local ok, write_err = snapshot_writer("/tmp/advanced-snapshot")
                assert(ok, write_err)
                table.insert(checkpoint_writes, { dir = dir, safe_block = safe_block, manifest = manifest })
                return true
            end,
        },
        safe_block = function()
            return 9
        end,
        fetch_inputs = function(from_block, to_block)
            assert_eq(from_block, 8)
            assert_eq(to_block, 9)
            return { { raw_input = "one" }, { raw_input = "two" } }
        end,
        machine = machine,
    })

    assert(result, err)
    assert_eq(result.safe_block, 9)
    assert_eq(result.input_count, 2)
    assert_eq(machine.saved_snapshot_dir, "/tmp/advanced-snapshot")
    assert_eq(#checkpoint_writes, 1)
end)

test("machine cli adapter writes raw input files", function()
    local base = os.tmpname()
    os.remove(base)
    os.execute(string.format('mkdir -p "%s"', base))
    local driver = machine_cli.new({ work_dir = base, executable = "cartesi-machine" })
    local instance = assert(driver:load("/tmp/source-snapshot"))
    assert(driver:feed_inputs(instance, {
        { raw_input = "abc" },
        { raw_input = "def" },
    }))

    local file = io.open(instance.input_dir .. "/input-0.bin", "rb")
    assert(file ~= nil, "first input file exists")
    assert_eq(file:read("*a"), "abc")
    file:close()
    file = io.open(instance.input_dir .. "/input-1.bin", "rb")
    assert(file ~= nil, "second input file exists")
    assert_eq(file:read("*a"), "def")
    file:close()
end)

test("machine cli adapter inspect reads report file", function()
    local base = os.tmpname()
    os.remove(base)
    os.execute(string.format('mkdir -p "%s"', base))
    local script_path = base .. "/fake-cartesi-machine.sh"
    local driver = machine_cli.new({ work_dir = base, executable = script_path })
    local instance = assert(driver:load("/tmp/source-snapshot"))

    local script = io.open(script_path, "wb")
    assert(script ~= nil, "fake cartesi-machine script opened")
    script:write(string.format([[
#!/bin/sh
mkdir -p "%s"
printf '%%s' '{"ok":true}' > "%s/inspect-report-0.bin"
exit 0
]], instance.work_dir, instance.work_dir))
    script:close()
    os.execute(string.format('chmod +x "%s"', script_path))

    local state, err = driver:inspect_state(instance)

    assert_eq(err, nil)
    assert_eq(state, '{"ok":true}')
end)

test("machine cli adapter leaves snapshot directory creation to cartesi-machine", function()
    local base = os.tmpname()
    os.remove(base)
    os.execute(string.format('mkdir -p "%s"', base))
    local driver = machine_cli.new({ work_dir = base, executable = "true" })
    local instance = assert(driver:load("/tmp/source-snapshot"))
    local snapshot_dir = base .. "/snapshot"

    assert(driver:save(instance, snapshot_dir))
    local exists = os.rename(snapshot_dir, snapshot_dir)
    assert(not exists, "adapter must not pre-create --store target")
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

test("alarm webhook posts JSON payload", function()
    local sent = {}
    local http = {}
    function http.post(_self, url, body, headers)
        sent.url = url
        sent.body = body
        sent.headers = headers
        return { status = 204, body = "" }
    end

    local ok, err = alarm.send_webhook(http, "http://alarm", {
        kind = "state_mismatch",
        safe_block = 12,
    })

    assert(ok, err)
    assert_eq(sent.url, "http://alarm")
    assert_eq(sent.headers["content-type"], "application/json")
    assert(sent.body:find('"kind":"state_mismatch"', 1, true) ~= nil, "body includes kind")
    assert(sent.body:find('"safe_block":12', 1, true) ~= nil, "body includes safe block")
end)

test("alarm webhook reports non-success status", function()
    local http = {}
    function http.post(_self, _url, _body, _headers)
        return { status = 500, body = "" }
    end

    local ok, err = alarm.send_webhook(http, "http://alarm", { kind = "test" })
    assert_eq(ok, nil)
    assert_eq(err, "alarm webhook HTTP 500")
end)

local failures = 0
for _, t in ipairs(tests) do
    local ok, err = pcall(t.fn)
    if ok then
        io.write("ok - " .. t.name .. "\n")
    else
        failures = failures + 1
        io.write("not ok - " .. t.name .. ": " .. tostring(err) .. "\n")
    end
end

if failures > 0 then
    os.exit(1)
end
