-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Shared test helpers: a tiny runner, temporary directories, EvmAdvance and
--- InputAdded encoders, and fakes for the watchdog's collaborators at their
--- module boundaries (machine facade, sequencer client, L1 reader).

local lfs = require("lfs")
local abi = require("watchdog.abi")
local errors = require("watchdog.errors")
local store_mod = require("watchdog.store")

local support = {}

-- ── Runner ────────────────────────────────────────────────────────────────

function support.runner()
    local tests = {}
    local runner = {}
    function runner.test(name, fn)
        tests[#tests + 1] = { name = name, fn = fn }
    end
    function runner.run(filter)
        local failed = 0
        for _, t in ipairs(tests) do
            if not filter or t.name:find(filter, 1, true) then
                local ok, err = xpcall(t.fn, debug.traceback)
                if ok then
                    io.write("ok   ", t.name, "\n")
                else
                    failed = failed + 1
                    io.write("FAIL ", t.name, "\n", tostring(err), "\n")
                end
            end
        end
        io.write(string.format("%d tests, %d failed\n", #tests, failed))
        return failed == 0
    end
    return runner
end

function support.eq(actual, expected, what)
    if actual ~= expected then
        error(string.format("%sexpected %s, got %s", what and (what .. ": ") or "",
            tostring(expected), tostring(actual)), 2)
    end
end

--- Run `fn`; it must raise an error of `class` whose message contains `text`.
function support.raises(class, text, fn)
    local ok, err = pcall(fn)
    if ok then
        error("expected a " .. class .. " error", 2)
    end
    local got_class, message = errors.classify(err)
    if got_class ~= class or (text and not message:find(text, 1, true)) then
        error(string.format("expected %s error containing %q, got %s: %s", class, tostring(text), got_class,
            message), 2)
    end
    return message
end

-- ── Files ─────────────────────────────────────────────────────────────────

function support.tmpdir()
    local path = os.tmpname()
    os.remove(path)
    assert(lfs.mkdir(path))
    return path
end

support.remove_tree = store_mod.remove_tree

function support.read(path)
    local file = io.open(path, "rb")
    if not file then
        return nil
    end
    local data = file:read("a")
    file:close()
    return data
end

function support.write(path, data)
    local file = assert(io.open(path, "wb"))
    assert(file:write(data))
    assert(file:close())
end

function support.exists(path)
    return lfs.attributes(path, "mode") ~= nil
end

-- ── Encoders ──────────────────────────────────────────────────────────────

local function word(n)
    return string.format("%064x", n)
end

local function address_word(address)
    return string.rep("0", 24) .. address:sub(3)
end

local function padded_bytes(bytes)
    local hex = abi.hex_from_bytes(bytes)
    local rem = #bytes % 32
    if rem > 0 then
        hex = hex .. string.rep("00", 32 - rem)
    end
    return word(#bytes) .. hex
end

support.CHAIN_ID = 31337
support.APP = "0x1111111111111111111111111111111111111111"
support.INPUT_BOX = "0x9999999999999999999999999999999999999999"

--- Raw `EvmAdvance` calldata, the bytes the canonical machine receives.
function support.evm_advance(fields)
    return abi.bytes_from_hex("415bf363"
        .. word(fields.chain_id or support.CHAIN_ID)
        .. address_word(fields.app or support.APP)
        .. address_word(fields.sender or "0x2222222222222222222222222222222222222222")
        .. word(fields.block)
        .. word(fields.timestamp or 0)
        .. word(0)
        .. word(fields.index)
        .. word(0x100)
        .. padded_bytes(fields.payload or ""))
end

--- An `InputAdded` log carrying `raw` in block `block`.
function support.input_added_log(raw, block, position)
    return {
        blockNumber = string.format("0x%x", block),
        transactionIndex = "0x0",
        logIndex = string.format("0x%x", position or 0),
        data = "0x" .. word(0x20) .. padded_bytes(raw),
    }
end

--- A fake JSON-RPC client over a list of `{ block, payload }` inputs of one
--- application, numbered in order.
function support.fake_rpc(inputs, opts)
    opts = opts or {}
    local rpc = { calls = {} }
    local logs = {}
    for index, input in ipairs(inputs) do
        local raw = support.evm_advance({ block = input.block, index = index - 1, payload = input.payload,
            chain_id = input.chain_id })
        logs[#logs + 1] = { block = input.block, log = support.input_added_log(raw, input.block, index) }
    end
    function rpc:get_logs(filter)
        self.calls[#self.calls + 1] = { filter.from_block, filter.to_block }
        if opts.fail and opts.fail(filter.from_block, filter.to_block) then
            return nil, opts.fail(filter.from_block, filter.to_block)
        end
        local out = {}
        for _, entry in ipairs(logs) do
            if entry.block >= filter.from_block and entry.block <= filter.to_block
                and not (opts.drop and opts.drop(entry.block)) then
                out[#out + 1] = entry.log
            end
        end
        return out
    end
    function rpc:chain_id()
        return opts.chain_id or support.CHAIN_ID
    end
    function rpc:block_number()
        return opts.safe_head or math.maxinteger
    end
    function rpc:get_code()
        return opts.input_box_code or "0x6080"
    end
    function rpc:eth_call(_, data, block)
        if data == "0xf02478de" then
            -- getDataAvailability(): DataAvailability.InputBox(INPUT_BOX), as `bytes`.
            local availability = opts.availability or ("b12c9ede" .. address_word(support.INPUT_BOX))
            return "0x" .. word(0x20) .. word(#availability // 2) .. availability .. string.rep("0", 56)
        end
        if data:sub(1, 10) == "0x61a93c87" then
            if opts.input_box_from and block < opts.input_box_from then
                return "0x"
            end
            local count = 0
            for _, entry in ipairs(logs) do
                if entry.block <= block then
                    count = count + 1
                end
            end
            return "0x" .. word(count)
        end
        return opts.template_hash or ("0x" .. string.rep("ab", 32))
    end
    return rpc
end

-- ── Fakes for tick, bootstrap, and main ──────────────────────────────────

--- A machine facade whose stored machines are directories holding a
--- `state` file. `script.statuses[i]` is the status of the i-th advanced
--- input (default `accepted`). `script.publish_error` makes `publish` raise it.
--- `sha256` is the identity, so fake digests are readable.
function support.fake_machine(script)
    script = script or {}
    local fake = { advanced = 0 }
    local function state_path(dir)
        return dir .. "/state"
    end
    function fake.open(head_dir, working_dir)
        assert(lfs.mkdir(working_dir))
        support.write(state_path(working_dir), support.read(state_path(head_dir)) or "")
        return {
            advance = function(_, raw)
                fake.advanced = fake.advanced + 1
                local status = (script.statuses or {})[fake.advanced] or { kind = "accepted" }
                if status.kind == "accepted" then
                    local payload = abi.decode_evm_advance(raw).payload
                    support.write(state_path(working_dir), support.read(state_path(working_dir)) .. payload:sub(-1))
                end
                return status
            end,
            close = function() end,
        }
    end
    function fake.state_bytes(dir)
        return script.state or support.read(state_path(dir))
    end
    function fake.sha256(bytes)
        return bytes
    end
    function fake.publish(from, to)
        if script.publish_error then
            error(script.publish_error, 0)
        end
        assert(os.rename(from, to))
    end
    function fake.clone(from, to)
        assert(lfs.mkdir(to))
        support.write(state_path(to), support.read(state_path(from)) or "")
    end
    function fake.sync() end
    function fake.describe(status)
        return status.kind
    end
    function fake.check_source()
        if script.bad_source then
            errors.operator("bad source")
        end
    end
    function fake.root_hash()
        return script.root_hash or string.rep("\171", 32)
    end
    return fake
end

--- A sequencer client that reports `block` with digest `sha256`, and serves
--- `bytes` as its comparison file.
function support.fake_sequencer(state)
    return {
        inclusion_block = function()
            return state.polled or state.block
        end,
        digest = function()
            return { inclusion_block = state.block, sha256 = state.sha256 }
        end,
        download_state = function(_, path)
            support.write(path, state.bytes or "")
            return state.served_block or state.block
        end,
    }
end

--- A bootstrapped state directory with its head at `block` holding `state`.
function support.state_dir(json, block, input_count, state)
    local dir = support.tmpdir()
    local store = store_mod.open(dir, json)
    local head = store:checkpoint_dir(block, input_count)
    assert(lfs.mkdir(head))
    support.write(head .. "/state", state or "")
    return store
end

return support
