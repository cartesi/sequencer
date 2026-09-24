-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- The canonical Cartesi Machine, driven in-process with the reference rollup
--- host semantics (docs/cartesi-machine.md#rollup-host-semantics): every input
--- runs on a clone-backed snapshot of the working checkpoint; a rejected input
--- restores the snapshot; any stop other than an accept is a permanent fixed
--- point. Checkpoints are stored machine directories published with fsync.

local errors = require("watchdog.errors")

local machine = {}

--- Guest console output goes to the watchdog's stderr: a dying guest's last
--- words are incident evidence.
local RUNTIME = { console = { output_destination = "to_stderr" } }

function machine.new(cartesi)
    cartesi = cartesi or require("cartesi")

    local SHARING_ALL = cartesi.SHARING_ALL
    local SHARING_NONE = cartesi.SHARING_NONE
    local MANUAL = cartesi.HTIF_YIELD_CMD_MANUAL
    local RX_ACCEPTED = cartesi.HTIF_YIELD_MANUAL_REASON_RX_ACCEPTED
    local RX_REJECTED = cartesi.HTIF_YIELD_MANUAL_REASON_RX_REJECTED
    local TX_EXCEPTION = cartesi.HTIF_YIELD_MANUAL_REASON_TX_EXCEPTION
    local TX_REPORT = cartesi.HTIF_YIELD_AUTOMATIC_REASON_TX_REPORT

    local api = {}

    --- Where a machine at rest stands, from register reads only. `accepted`
    --- is the only state that can take another request.
    local function status(m)
        if not math.ult(m:read_reg("mcycle"), m:read_reg("imcyclemax")) then
            return { kind = "mcycle_overflow" }
        end
        if m:read_reg("iflags_H") ~= 0 then
            return { kind = "halted", exit_code = m:read_reg("htif_tohost_data") >> 1 }
        end
        if m:read_reg("iflags_Y") ~= 0 then
            local cmd, reason, data = m:receive_cmio_request()
            if cmd == MANUAL and reason == RX_ACCEPTED then
                return { kind = "accepted" }
            elseif cmd == MANUAL and reason == RX_REJECTED then
                return { kind = "rejected" }
            elseif cmd == MANUAL and reason == TX_EXCEPTION then
                return { kind = "exception", message = data }
            end
            return { kind = "unexpected_yield", cmd = cmd, reason = reason }
        end
        return { kind = "running" }
    end

    local function describe(st)
        if st.kind == "halted" then
            return "halt with exit code " .. tostring(st.exit_code)
        elseif st.kind == "exception" then
            return string.format("exception %q", tostring(st.message))
        elseif st.kind == "unexpected_yield" then
            return string.format("unexpected yield (cmd %s, reason %s)", tostring(st.cmd), tostring(st.reason))
        end
        return st.kind
    end
    api.describe = describe

    --- Run until the machine stops for good or yields manually, passing each
    --- automatic yield's reason and data to `on_automatic`.
    local function run_to_stop(m, on_automatic)
        while true do
            local reason = m:run(cartesi.MCYCLE_MAX)
            if reason == cartesi.BREAK_REASON_HALTED
                or reason == cartesi.BREAK_REASON_YIELDED_MANUALLY
                or reason == cartesi.BREAK_REASON_MCYCLE_OVERFLOW
            then
                return
            elseif reason == cartesi.BREAK_REASON_YIELDED_AUTOMATICALLY then
                if on_automatic then
                    local _, yield_reason, data = m:receive_cmio_request()
                    on_automatic(yield_reason, data)
                end
            elseif reason ~= cartesi.BREAK_REASON_YIELDED_SOFTLY
                and reason ~= cartesi.BREAK_REASON_CONSOLE_OUTPUT
            then
                error("machine run stopped for unexpected break reason " .. tostring(reason))
            end
        end
    end

    local function load(dir, sharing)
        local m = cartesi.new()
        local ok, err = pcall(m.load, m, dir, RUNTIME, sharing)
        if not ok then
            errors.operator("cannot load stored machine %s: %s", dir, tostring(err))
        end
        return m
    end

    local function require_accepted(m, dir)
        local st = status(m)
        if st.kind ~= "accepted" then
            errors.operator("stored machine %s is at %s, not waiting for an input", dir, describe(st))
        end
    end

    --- Copy a stored machine cheaply (reflinks where the filesystem allows).
    function api.clone(from_dir, to_dir)
        cartesi.new():clone_stored(from_dir, to_dir)
    end

    --- Make `from_dir` durable and atomically publish it as `to_dir`.
    function api.publish(from_dir, to_dir)
        local m = cartesi.new()
        m:sync_stored(from_dir)
        m:rename_stored(from_dir, to_dir)
    end

    function api.root_hash(dir)
        local m = load(dir, SHARING_NONE)
        local root = m:get_root_hash()
        m:destroy()
        return root
    end

    --- Make a stored machine directory durable in place.
    function api.sync(dir)
        cartesi.new():sync_stored(dir)
    end

    --- The state range a user label names: a flash drive or NVRAM.
    function api.resolve_range(config, label)
        local found
        for _, kind in ipairs({ "flash_drive", "nvram" }) do
            for _, range in ipairs(config[kind] or {}) do
                if range.label == label then
                    if found then
                        errors.operator("label %q names more than one memory range", label)
                    end
                    found = { start = range.start, length = range.length }
                end
            end
        end
        if not found then
            errors.operator("no flash drive or NVRAM carries the label %q", label)
        end
        return found
    end

    local function inspect_state(m)
        m:send_cmio_response(cartesi.HTIF_YIELD_REASON_INSPECT_STATE, "state")
        local reports = {}
        run_to_stop(m, function(reason, data)
            if reason == TX_REPORT then
                reports[#reports + 1] = data
            end
        end)
        local st = status(m)
        if st.kind ~= "accepted" then
            errors.operator("inspect query 'state' ended at %s", describe(st))
        end
        if #reports ~= 1 then
            errors.operator("inspect query 'state' produced %d reports; the contract is exactly one", #reports)
        end
        return reports[1]
    end

    --- The application state bytes of a stored machine that waits for an
    --- input. The machine is opened privately, so the inspect query and any
    --- other execution here never reach the stored files.
    function api.state_bytes(dir, source)
        local m = load(dir, SHARING_NONE)
        require_accepted(m, dir)
        local bytes
        if source.kind == "range" then
            local range = api.resolve_range(m:get_initial_config(), source.label)
            bytes = m:read_memory(range.start, range.length)
        else
            bytes = inspect_state(m)
        end
        m:destroy()
        return bytes
    end

    --- Fail unless a stored machine waits for an input and can yield the
    --- state source: the label resolves, or the inspect query answers.
    function api.check_source(dir, source)
        local m = load(dir, SHARING_NONE)
        require_accepted(m, dir)
        if source.kind == "range" then
            api.resolve_range(m:get_initial_config(), source.label)
        else
            inspect_state(m)
        end
        m:destroy()
    end

    --- Lowercase hex SHA-256, matching the sequencer's digest route.
    function api.sha256(bytes)
        return (cartesi.sha256(bytes):gsub(".", function(c)
            return string.format("%02x", c:byte())
        end))
    end

    --- An executor advances a private working clone of `head_dir` in place.
    --- Each input snapshots the working directory first (`<working>.revert`),
    --- exactly like the reference CLI's `--revert-mode=stored`.
    local Executor = {}
    Executor.__index = Executor

    function api.open(head_dir, working_dir)
        api.clone(head_dir, working_dir)
        local m = load(working_dir, SHARING_ALL)
        require_accepted(m, head_dir)
        return setmetatable({
            m = m,
            working = working_dir,
            snapshot = working_dir .. ".revert",
        }, Executor)
    end

    --- Feed one raw EvmAdvance input. Returns the resulting status: `accepted`,
    --- `rejected` (state restored), or a fixed point left in place.
    function Executor:advance(input)
        local m = self.m
        local revert_root = m:get_root_hash()
        m:destroy()
        m:clone_stored(self.working, self.snapshot)
        m:load(self.working, RUNTIME, SHARING_ALL)
        m:send_cmio_response(cartesi.HTIF_YIELD_REASON_ADVANCE_STATE, input, revert_root)
        run_to_stop(m)
        local st = status(m)
        if st.kind == "rejected" then
            m:destroy()
            m:remove_stored(self.working)
            m:rename_stored(self.snapshot, self.working)
            m:load(self.working, RUNTIME, SHARING_ALL)
            local restored = m:get_root_hash()
            assert(restored == revert_root, "restored snapshot does not match the input's revert root hash")
        else
            -- Accepted state is committed; a fixed point is permanent, so its
            -- pre-input snapshot is never needed again.
            m:remove_stored(self.snapshot)
        end
        return st
    end

    function Executor:close()
        self.m:destroy()
    end

    return api
end

return machine
