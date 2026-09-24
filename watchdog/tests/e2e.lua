-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Watchdog tests against the real Cartesi Machine. The test guest
--- (watchdog/test-guest) drives every rollup host outcome; the reference
--- `cartesi-machine` CLI is the oracle for the executor. Run from the repo
--- root with the canonical and test guest images built:
---   lua5.4 watchdog/tests/e2e.lua [name filter]

package.path = "./?.lua;./?/init.lua;" .. package.path
package.cpath = (os.getenv("CARTESI_WATCHDOG_LUA_DEPS") or ".deps/lua") .. "/?.so;" .. package.cpath

local abi = require("watchdog.abi")
local bootstrap = require("watchdog.bootstrap")
local config = require("watchdog.config")
local incident = require("watchdog.incident")
local json = require("watchdog.json").new()
local l1_mod = require("watchdog.l1")
local machine = require("watchdog.machine").new()
local replay = require("watchdog.replay")
local store_mod = require("watchdog.store")
local support = require("watchdog.tests.support")
local tick = require("watchdog.tick")

local GUEST_IMAGE = "watchdog/test-guest/out/test-machine-image"
local WALLET_IMAGE = "examples/canonical-app/out/canonical-machine-image-sepolia"
local STATE_LENGTH = 64 * 1024

local function absolute(path)
    local dir = io.popen("pwd"):read("l")
    return dir .. "/" .. path
end

local function require_image(path)
    assert(support.exists(path .. "/config.json"), "missing machine image " .. path
        .. "; build it (watchdog/test-guest/justfile build-image, or just canonical-build-machine-image-sepolia)")
    return absolute(path)
end

local runner = support.runner()
local test, eq, raises = runner.test, support.eq, support.raises

local function now()
    return "2026-09-22T12:00:00Z"
end

--- The raw input the i-th (0-based) test input carries.
local function input(i, payload)
    return support.evm_advance({ block = i + 1, index = i, payload = payload })
end

--- The guest's NVRAM after accepting `payloads` (see test-guest/README.md).
local function guest_state(payloads)
    local total, last = 0, ""
    for _, payload in ipairs(payloads) do
        total, last = total + #payload, payload
    end
    local head = string.pack("<I8I8I4", #payloads, total, #last) .. last
    return head .. string.rep("\0", STATE_LENGTH - #head)
end

local function hex(bytes)
    return abi.hex_from_bytes(bytes)
end

--- Run `payloads` through our executor from `image`; returns the statuses and
--- the working machine's root hash.
local function execute(image, payloads)
    local work = support.tmpdir()
    local executor = machine.open(image, work .. "/working")
    local statuses = {}
    for i, payload in ipairs(payloads) do
        local status = executor:advance(input(i - 1, payload))
        statuses[i] = status
        if status.kind ~= "accepted" and status.kind ~= "rejected" then
            break
        end
    end
    executor:close()
    return statuses, machine.root_hash(work .. "/working"), work .. "/working"
end

--- The reference CLI's final root hash for the same inputs.
local function oracle(image, payloads)
    local dir = support.tmpdir()
    for i, payload in ipairs(payloads) do
        support.write(string.format("%s/input-%d.bin", dir, i - 1), input(i - 1, payload))
    end
    local command = string.format("cartesi-machine --load=%s --remote-spawn --remote-shutdown "
        .. "--cmio-advance-state=input:%s/input-%%i.bin,input_index_begin:0,input_index_end:%d,"
        .. "check_outputs_merkle_root:false,output:,rejected_output:,output_proof:,report:,"
        .. "outputs_merkle_root:,outputs_merkle_root_proof: --final-hash=%s/final.bin >%s/cli.log 2>&1",
        image, dir, #payloads, dir, dir)
    os.execute(command)
    local final = support.read(dir .. "/final.bin")
    assert(final and #final == 32, "CLI oracle produced no final hash; see " .. dir .. "/cli.log")
    return final
end

-- ── Executor against the reference CLI ───────────────────────────────────

test("executor matches the CLI when inputs are accepted", function()
    local image = require_image(GUEST_IMAGE)
    local payloads = { "hello", "world!" }
    local statuses, root = execute(image, payloads)
    eq(statuses[2].kind, "accepted")
    eq(hex(root), hex(oracle(image, payloads)))
end)

test("executor reverts a rejected input exactly like the CLI", function()
    local image = require_image(GUEST_IMAGE)
    local statuses, root, working = execute(image, { "hello", "reject", "world!" })
    eq(statuses[2].kind, "rejected")
    eq(statuses[3].kind, "accepted")
    eq(hex(root), hex(oracle(image, { "hello", "reject", "world!" })))
    -- The guest scribbles 0xFF before rejecting; none of it may survive.
    eq(machine.state_bytes(working, { kind = "range", label = "state" }), guest_state({ "hello", "world!" }))
end)

test("executor leaves an exception as a permanent fixed point, like the CLI", function()
    local image = require_image(GUEST_IMAGE)
    local statuses, root, working = execute(image, { "hello", "exception", "world!" })
    eq(#statuses, 2)
    eq(statuses[2].kind, "exception")
    eq(statuses[2].message, "test-guest exception")
    eq(hex(root), hex(oracle(image, { "hello", "exception", "world!" })))
    raises("operator", "not waiting for an input", function()
        machine.state_bytes(working, { kind = "range", label = "state" })
    end)
end)

test("executor reports a halt with the guest's exit code, like the CLI", function()
    local image = require_image(GUEST_IMAGE)
    local statuses, root = execute(image, { "hello", "halt", "world!" })
    eq(statuses[2].kind, "halted")
    eq(statuses[2].exit_code, 7)
    eq(hex(root), hex(oracle(image, { "hello", "halt", "world!" })))
end)

-- ── State sources ─────────────────────────────────────────────────────────

test("range and inspect sources read the same guest state", function()
    local image = require_image(GUEST_IMAGE)
    local _, _, working = execute(image, { "hello", "report me", "world!" })
    local range = machine.state_bytes(working, { kind = "range", label = "state" })
    eq(range, guest_state({ "hello", "report me", "world!" }))
    eq(machine.state_bytes(working, { kind = "inspect" }), range)
    -- Inspecting runs on a private copy; the stored machine is untouched.
    local before = machine.root_hash(working)
    machine.state_bytes(working, { kind = "inspect" })
    eq(hex(machine.root_hash(working)), hex(before))
end)

test("an unknown range label is an operator error", function()
    local image = require_image(GUEST_IMAGE)
    raises("operator", 'no flash drive or NVRAM carries the label "missing"', function()
        machine.check_source(image, { kind = "range", label = "missing" })
    end)
end)

test("the canonical wallet image answers the inspect query with its golden genesis state", function()
    local image = require_image(WALLET_IMAGE)
    local golden = abi.bytes_from_hex(support.read("tests/fixtures/wallet_snapshot_empty.hex"):gsub("%s", ""))
    eq(hex(machine.state_bytes(image, { kind = "inspect" })), hex(golden))
end)

-- ── Full commands over the test guest ─────────────────────────────────────

local function deployment(inputs, source)
    local image = require_image(GUEST_IMAGE)
    local rpc = support.fake_rpc(inputs, { template_hash = "0x" .. hex(machine.root_hash(image)) })
    local l1 = l1_mod.new(rpc, { input_box_address = support.INPUT_BOX, app_address = support.APP,
        chain_id = support.CHAIN_ID })
    local store = store_mod.open(support.tmpdir(), json)
    local cfg = {
        state_dir = store.dir,
        sequencer_url = "http://sequencer",
        input_box_address = support.INPUT_BOX,
        app_address = support.APP,
        state_source = config.parse_state_source(source or "range:state"),
        long_block_range_error_codes = l1_mod.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES,
        bootstrap_dir = image,
        bootstrap_block = 0,
    }
    bootstrap.run(cfg, { store = store, machine = machine, l1 = l1 })
    return cfg, store, l1
end

local function guest_inputs(...)
    local inputs = {}
    for i, payload in ipairs({ ... }) do
        inputs[i] = { block = i, payload = payload }
    end
    return inputs
end

--- One tick; a divergence's evidence is collected afterwards, as `main` does.
local function run_tick(cfg, store, l1, sequencer_state)
    local outcome = tick.run(cfg, { store = store, machine = machine, l1 = l1, now = now,
        sequencer = support.fake_sequencer(sequencer_state) })
    if outcome.evidence then
        incident.collect(store, outcome.incident, outcome.evidence)
    end
    return outcome
end

local function digest(payloads)
    return machine.sha256(guest_state(payloads))
end

test("tick agrees, advances the durable head, and prunes the old one", function()
    local cfg, store, l1 = deployment(guest_inputs("hello", "reject", "world!"))
    local first = store:head()
    local outcome = run_tick(cfg, store, l1, { block = 3, sha256 = digest({ "hello", "world!" }) })
    eq(outcome.kind, "agreed")
    eq(outcome.head.input_count, 3)
    eq(#store:checkpoints(), 1)
    eq(support.exists(first.dir), false)
    eq(run_tick(cfg, store, l1, { block = 3 }).kind, "idle")
end)

test("tick agrees through the inspect source", function()
    local cfg, store, l1 = deployment(guest_inputs("hello"), "inspect")
    eq(run_tick(cfg, store, l1, { block = 1, sha256 = digest({ "hello" }) }).kind, "agreed")
end)

test("tick latches a mismatch with the canonical machine and a byte diff", function()
    local cfg, store, l1 = deployment(guest_inputs("hello", "world!"))
    local wrong = guest_state({ "hello", "WORLD!" })
    local outcome = run_tick(cfg, store, l1, { block = 2, sha256 = machine.sha256(wrong), bytes = wrong })
    eq(outcome.kind, "diverged")
    local marker = incident.marker(store)
    eq(marker.kind, "state_mismatch")
    eq(marker.canonical_sha256, digest({ "hello", "world!" }))
    local evidence = incident.evidence(store)
    eq(evidence.collection, "finished")
    eq(evidence.comparison.first_difference, 20)
    eq(evidence.comparison.differing_pages, 1)
    eq(machine.state_bytes(store:path("incident", "canonical"), cfg.state_source), guest_state({ "hello", "world!" }))
    eq(store:head().block, 0)
end)

test("tick latches a dead canonical machine and replay reproduces the stop", function()
    local cfg, store, l1 = deployment(guest_inputs("hello", "halt", "world!"))
    eq(run_tick(cfg, store, l1, { block = 3, sha256 = digest({ "hello", "world!" }) }).kind, "diverged")
    local marker = incident.marker(store)
    eq(incident.evidence(store).canonical_machine, "incident/canonical")
    eq(marker.kind, "canonical_machine_dead")
    eq(marker.stop.status, "halted")
    eq(marker.stop.description, "halt with exit code 7")
    eq(marker.stop.input_index, 1)

    local out = support.tmpdir() .. "/replayed"
    local result = replay.run({ chain_id = support.CHAIN_ID, state_source = cfg.state_source },
        { machine = machine, l1 = l1 },
        { from_dir = cfg.bootstrap_dir, from_block = 0, to_block = 3, out_dir = out })
    eq(result.stop.input_index, 1)
end)

test("replay re-derives the digest a tick agreed on", function()
    local cfg, store, l1 = deployment(guest_inputs("hello", "reject", "world!"))
    run_tick(cfg, store, l1, { block = 3, sha256 = digest({ "hello", "world!" }) })
    local out = support.tmpdir() .. "/replayed"
    local result = replay.run({ chain_id = support.CHAIN_ID, state_source = cfg.state_source },
        { machine = machine, l1 = l1 },
        { from_dir = cfg.bootstrap_dir, from_block = 0, to_block = 3, out_dir = out })
    eq(result.sha256, digest({ "hello", "world!" }))
    eq(result.input_count, 3)
end)

test("init refuses a bootstrap machine that is not the template", function()
    local image = require_image(GUEST_IMAGE)
    local rpc = support.fake_rpc({}, { template_hash = "0x" .. string.rep("00", 32) })
    local store = store_mod.open(support.tmpdir(), json)
    raises("operator", "differs from the on-chain template hash", function()
        bootstrap.run({
            state_source = { kind = "range", label = "state" },
            input_box_address = support.INPUT_BOX,
            app_address = support.APP,
            long_block_range_error_codes = l1_mod.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES,
            bootstrap_dir = image,
            bootstrap_block = 0,
        }, { store = store, machine = machine, l1 = l1_mod.new(rpc, { input_box_address = support.INPUT_BOX,
            app_address = support.APP, chain_id = support.CHAIN_ID }) })
    end)
end)

os.exit(runner.run(arg[1]) and 0 or 1)
