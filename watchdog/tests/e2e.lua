-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Watchdog tests against the real Cartesi Machine: the test guest and sling's
--- bare-metal seam machines drive the rollup host outcomes. The reference
--- `cartesi-machine` CLI is the oracle, and the step's rule where the CLI aborts
--- or departs from it.
--- Run from the repo root with the canonical and test guest images built:
---   lua5.4 watchdog/tests/e2e.lua [name filter]

package.path = "./?.lua;./?/init.lua;" .. package.path
package.cpath = (os.getenv("CARTESI_WATCHDOG_LUA_DEPS") or ".deps/lua") .. "/?.so;" .. package.cpath

local abi = require("watchdog.abi")
local bootstrap = require("watchdog.bootstrap")
local cartesi = require("cartesi")
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
        .. "--cmio-advance-state=input:%s/input-%%i.bin,input_file_index_begin:0,input_file_index_end:%d,"
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

-- ── Bare-metal seams ──────────────────────────────────────────────────────

--- The seam guest of sling's `MachineStf` tests (dave@dbcca7d4): it clears
--- fromhost, yields the tohost word in x19, then yields x20 each time it
--- resumes. With imcyclemax = 3 the x19 yield lands on the budget's last cycle.
local SEAM_PROGRAM = string.pack("<I4I4I4I4I4",
    0x400082b7, -- lui t0, 0x40008 (HTIF base)
    0x0002b423, -- sd zero, 8(t0) (fromhost)
    0x0132b023, -- sd x19, 0(t0) (tohost)
    0x0142b023, -- sd x20, 0(t0) (tohost)
    0xffdff06f) -- j -4

local RX_ACCEPTED = cartesi.HTIF_YIELD_MANUAL_REASON_RX_ACCEPTED
local RX_REJECTED = cartesi.HTIF_YIELD_MANUAL_REASON_RX_REJECTED
local TX_EXCEPTION = cartesi.HTIF_YIELD_MANUAL_REASON_TX_EXCEPTION

--- A whole tohost word, declaring `length` bytes of data.
local function tohost(dev, cmd, reason, length)
    return (dev << cartesi.HTIF_DEV_SHIFT) | (cmd << cartesi.HTIF_CMD_SHIFT) | (reason << cartesi.HTIF_REASON_SHIFT)
        | (length or 0)
end

local function manual_yield(reason, length)
    return tohost(cartesi.HTIF_DEV_YIELD, cartesi.HTIF_YIELD_CMD_MANUAL, reason, length)
end

--- A stored machine with 4 KiB of RAM and no image, created with
--- `registers` and run from the start of RAM until `program` stops.
local function bare_machine(registers, program)
    local m = cartesi.new():create({ ram = { length = 4096 }, processor = { registers = registers } })
    if program then
        m:write_memory(cartesi.AR_RAM_START, program)
        m:run(cartesi.MCYCLE_MAX)
    end
    local dir = support.tmpdir() .. "/machine"
    m:store(dir)
    m:destroy()
    return dir
end

local function seam_machine(imcyclemax, x19, x20)
    return bare_machine({ pc = cartesi.AR_RAM_START, imcyclemax = imcyclemax, x19 = x19, x20 = x20 },
        SEAM_PROGRAM)
end

test("executor takes inputs after an acceptance on the budget's last cycle, like the CLI", function()
    -- An accept declares its 32-byte outputs root, which the CLI reads.
    local accepted = manual_yield(RX_ACCEPTED, cartesi.HASH_SIZE)
    local dir = seam_machine(3, accepted, accepted)
    local work = support.tmpdir()
    local executor = machine.open(dir, work .. "/working")
    local mcycles = {}
    for i = 1, 2 do
        eq(executor:advance(input(i - 1, "seam")).kind, "accepted")
        mcycles[i] = executor.m:read_reg("mcycle")
    end
    executor:close()
    -- Delivery renewed the budget: the first input ran the x20 yield, the
    -- second jumped back and yielded again.
    eq(table.concat(mcycles, " "), "4 6")
    eq(hex(machine.root_hash(work .. "/working")), hex(oracle(dir, { "seam", "seam" })))
end)

for _, case in ipairs({
    { stop = "whose budget runs out before its yield", imcyclemax = 2, x19 = RX_ACCEPTED,
        refusal = "is at mcycle_overflow" },
    { stop = "at an exception on its budget's last cycle", imcyclemax = 3, x19 = TX_EXCEPTION,
        refusal = "is at exception" },
    { stop = "at an unexpected yield on its budget's last cycle", imcyclemax = 3, x19 = 0x7f,
        refusal = "is at unexpected yield" },
    { stop = "at a rejection on its budget's last cycle", imcyclemax = 3, x19 = RX_REJECTED,
        refusal = "is at rejected" },
}) do
    test("open refuses a seam machine " .. case.stop, function()
        local dir = seam_machine(case.imcyclemax, manual_yield(case.x19), manual_yield(RX_ACCEPTED))
        raises("operator", case.refusal, function()
            machine.open(dir, support.tmpdir() .. "/working")
        end)
    end)
end

test("executor fails loud on an input the machine does not take", function()
    local dir = seam_machine(cartesi.MCYCLE_MAX, manual_yield(RX_ACCEPTED), manual_yield(TX_EXCEPTION))
    local executor = machine.open(dir, support.tmpdir() .. "/working")
    eq(executor:advance(input(0, "seam")).kind, "exception")
    -- An advance at an exception is the emulator's silent no-op.
    raises("internal", "did not take", function()
        executor:advance(input(1, "seam"))
    end)
    executor:close()
end)

--- A tohost `word` declaring one byte past the TX buffer, which a guest that
--- bypasses libcmt can yield and `receive_cmio_request` refuses. The CLI
--- aborts reading it, so the step's rule is the oracle: an accept takes the
--- next input, and a reject restores the input's revert root hash.
local function oversized(word)
    return word | ((1 << cartesi.AR_CMIO_TX_BUFFER_LOG2_SIZE) + 1)
end

test("executor takes inputs at accepts that declare data past the TX buffer, as the step does", function()
    local accepted = oversized(manual_yield(RX_ACCEPTED))
    local executor = machine.open(seam_machine(cartesi.MCYCLE_MAX, accepted, accepted),
        support.tmpdir() .. "/working")
    eq(executor:advance(input(0, "seam")).kind, "accepted")
    eq(executor:advance(input(1, "seam")).kind, "accepted")
    executor:close()
end)

test("executor reverts a rejection that declares data past the TX buffer, as the step does", function()
    local dir = seam_machine(cartesi.MCYCLE_MAX, manual_yield(RX_ACCEPTED), oversized(manual_yield(RX_REJECTED)))
    local working = support.tmpdir() .. "/working"
    local executor = machine.open(dir, working)
    eq(executor:advance(input(0, "seam")).kind, "rejected")
    executor:close()
    eq(hex(machine.root_hash(working)), hex(machine.root_hash(dir)))
end)

test("executor stops at an exception that declares data past the TX buffer", function()
    local dir = seam_machine(cartesi.MCYCLE_MAX, manual_yield(RX_ACCEPTED), oversized(manual_yield(TX_EXCEPTION)))
    local executor = machine.open(dir, support.tmpdir() .. "/working")
    local status = executor:advance(input(0, "seam"))
    executor:close()
    eq(status.kind, "exception")
    eq(status.message, "<unavailable: declared length 2097153>")
end)

-- Execution sets iflags.Y only at a manual yield; only a template presets it
-- under another command or device, and the step treats that as a fixed point.
for _, case in ipairs({
    { header = "an automatic yield's command",
        tohost = tohost(cartesi.HTIF_DEV_YIELD, cartesi.HTIF_YIELD_CMD_AUTOMATIC, RX_ACCEPTED) },
    { header = "the console device",
        tohost = tohost(cartesi.HTIF_DEV_CONSOLE, cartesi.HTIF_YIELD_CMD_MANUAL, RX_ACCEPTED) },
}) do
    test("open refuses a preset yield under " .. case.header, function()
        local dir = bare_machine({ iflags = { Y = 1 }, htif = { tohost = case.tohost } })
        raises("operator", "is at unexpected yield", function()
            machine.open(dir, support.tmpdir() .. "/working")
        end)
    end)
end

test("executor delivers an input to a preset halted machine with an accept pending, as the step does", function()
    local dir = bare_machine({ iflags = { H = 1, Y = 1 }, htif = { tohost = manual_yield(RX_ACCEPTED) } })
    local raw = input(0, "seam")
    local working = support.tmpdir() .. "/working"
    local executor = machine.open(dir, working)
    local status = executor:advance(raw)
    executor:close()
    eq(status.kind, "halted")
    eq(status.exit_code, nil)
    -- The CLI ends the epoch at the halt, so the step's delivery is the oracle.
    local m = cartesi.new()
    m:load(dir, {}, cartesi.SHARING_NONE)
    local before = m:get_root_hash()
    m:send_cmio_response(cartesi.HTIF_YIELD_REASON_ADVANCE_STATE, raw, before)
    local delivered = m:get_root_hash()
    m:destroy()
    assert(delivered ~= before, "the oracle did not deliver the input")
    eq(hex(machine.root_hash(working)), hex(delivered))
    -- An inspect query cannot run on a halted machine.
    raises("operator", "inspect query 'state' ended at halt", function()
        machine.check_source(dir, { kind = "inspect" })
    end)
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

test("a stored machine with stale hash sidecars fails verification", function()
    local image = require_image(WALLET_IMAGE)
    local dir = support.tmpdir()
    local copy = dir .. "/machine"
    machine.clone(image, copy)
    machine.verify(copy)
    -- Replace the clone's RAM file to preserve the source even if storage
    -- sharing changes. Leave all cached hash files untouched.
    local path = copy .. "/0000000080000000-8000000.bin"
    local ram_input = assert(io.open(path, "rb"))
    local output = assert(io.open(path .. ".tmp", "wb"))
    local byte = assert(ram_input:read(1)):byte()
    output:write(string.char(byte ~ 1))
    while true do
        local chunk = ram_input:read(1024 * 1024)
        if not chunk then break end
        output:write(chunk)
    end
    ram_input:close()
    output:close()
    assert(os.rename(path .. ".tmp", path))
    eq(hex(machine.root_hash(copy)), hex(machine.root_hash(image)), "cached root alone misses corruption")
    raises("operator", "hash-tree verification", function() machine.verify(copy) end)
    support.remove_tree(dir)
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

test("init refuses a bootstrap machine the tick cannot run in place", function()
    local image = support.tmpdir() .. "/image"
    machine.clone(require_image(GUEST_IMAGE), image)
    -- Only the RAM file: the clone hard-links the image's read-only PMAs file,
    -- so a recursive chmod would change the shared test guest image.
    local ram = image .. "/0000000080000000-*.bin"
    assert(os.execute("chmod a-w " .. ram))
    local rpc = support.fake_rpc({}, { template_hash = "0x" .. hex(machine.root_hash(image)) })
    local store = store_mod.open(support.tmpdir(), json)
    local ok, err = pcall(bootstrap.run, {
        state_source = { kind = "range", label = "state" },
        input_box_address = support.INPUT_BOX,
        app_address = support.APP,
        bootstrap_dir = image,
        bootstrap_block = 0,
    }, { store = store, machine = machine, l1 = l1_mod.new(rpc, { input_box_address = support.INPUT_BOX,
        app_address = support.APP, chain_id = support.CHAIN_ID }) })
    assert(os.execute("chmod u+w " .. ram))
    eq(ok, false)
    local message = raises("operator", "chmod -R u+w " .. image, function()
        error(err, 0)
    end)
    -- Root bypasses file modes, so the emulator refuses the mode itself.
    assert(message:find("Permission denied", 1, true) or message:find("cannot be shared for writing", 1, true),
        message)
    eq(store:exists("config.json"), false)
    eq(#store:checkpoints(), 0)
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
            bootstrap_dir = image,
            bootstrap_block = 0,
        }, { store = store, machine = machine, l1 = l1_mod.new(rpc, { input_box_address = support.INPUT_BOX,
            app_address = support.APP, chain_id = support.CHAIN_ID }) })
    end)
end)

os.exit(runner.run(arg[1]) and 0 or 1)
