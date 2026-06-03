-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- In-process Cartesi Machine binding via the `cartesi` Lua module (ships with cartesi-machine).

local machine_runner = require("watchdog.machine_runner")

local machine_cartesi = {}

local STATE_INSPECT_QUERY = "state"
local MAX_MCYCLE = (1 << 53) - 1

local function require_cartesi()
    local ok, cartesi = pcall(require, "cartesi")
    if not ok then
        error(
            "cartesi Lua module is required; ensure cartesi-machine is installed and LUA_PATH includes its scripts"
        )
    end
    return cartesi
end

local function run_loop(machine, cartesi, handlers)
    while true do
        machine:run(MAX_MCYCLE)
        if machine:read_reg("iflags_H") ~= 0 then
            local exit_code = machine:read_reg("htif_tohost_data") >> 1
            return nil, "machine halted with exit code " .. tostring(exit_code)
        end
        if machine:read_reg("iflags_Y") ~= 0 then
            local cmd, reason, data = machine:receive_cmio_request()
            local err = handlers.on_manual(cmd, reason, data)
            if err then
                return nil, err
            end
            if machine:read_reg("iflags_Y") ~= 0 then
                return true
            end
        elseif machine:read_reg("iflags_X") ~= 0 then
            local cmd, reason, data = machine:receive_cmio_request()
            handlers.on_automatic(cmd, reason, data)
        end
    end
end

local function advance_inputs_on_machine(machine, cartesi, inputs)
    local queue = {}
    for _, input in ipairs(inputs) do
        table.insert(queue, input)
    end
    local next_index = 0
    local outputs_done = true

    local handlers = {
        on_manual = function(cmd, reason, data)
            if reason == cartesi.CMIO_YIELD_MANUAL_REASON_TX_EXCEPTION then
                return "CMIO exception: " .. tostring(data)
            end
            if next_index < #queue then
                if reason ~= cartesi.CMIO_YIELD_MANUAL_REASON_RX_ACCEPTED
                    and reason ~= cartesi.CMIO_YIELD_MANUAL_REASON_RX_REJECTED
                then
                    return "unexpected manual yield before feeding input"
                end
                next_index = next_index + 1
                local raw = queue[next_index].raw_input
                if type(raw) ~= "string" then
                    return "input.raw_input is required"
                end
                machine:send_cmio_response(cartesi.CMIO_YIELD_REASON_ADVANCE_STATE, raw)
                outputs_done = false
                return nil
            end
            if not outputs_done then
                if reason == cartesi.CMIO_YIELD_MANUAL_REASON_RX_ACCEPTED
                    or reason == cartesi.CMIO_YIELD_MANUAL_REASON_RX_REJECTED
                then
                    outputs_done = true
                end
                return nil
            end
            return nil
        end,
        on_automatic = function(_cmd, reason, data)
            if not outputs_done and reason == cartesi.CMIO_YIELD_AUTOMATIC_REASON_TX_OUTPUT then
                return
            end
            if not outputs_done and reason == cartesi.CMIO_YIELD_AUTOMATIC_REASON_TX_REPORT then
                return
            end
        end,
    }

    local ok, err = run_loop(machine, cartesi, handlers)
    if not ok then
        return nil, err
    end
    if next_index < #queue then
        return nil, "machine stopped before all inputs were fed"
    end
    return true
end

local function inspect_on_machine(machine, cartesi)
    local reports = {}
    local query_sent = false
    local query_done = false

    local handlers = {
        on_manual = function(_cmd, reason, _data)
            if not query_sent then
                machine:send_cmio_response(cartesi.CMIO_YIELD_REASON_INSPECT_STATE, STATE_INSPECT_QUERY)
                query_sent = true
                return nil
            end
            if reason == cartesi.CMIO_YIELD_MANUAL_REASON_RX_ACCEPTED
                or reason == cartesi.CMIO_YIELD_MANUAL_REASON_RX_REJECTED
            then
                query_done = true
            end
            return nil
        end,
        on_automatic = function(_cmd, reason, data)
            if query_sent and not query_done and reason == cartesi.CMIO_YIELD_AUTOMATIC_REASON_TX_REPORT then
                table.insert(reports, data)
            end
        end,
    }

    local ok, err = run_loop(machine, cartesi, handlers)
    if not ok then
        return nil, err
    end
    if #reports == 0 then
        return nil, "inspect produced no report"
    end
    return reports[1]
end

function machine_cartesi.new(_opts)
    local cartesi = require_cartesi()
    local runtime_config = {
        skip_root_hash_check = true,
        skip_root_hash_store = true,
    }

    local binding = {}

    function binding.load_snapshot(snapshot_dir)
        assert(type(snapshot_dir) == "string" and snapshot_dir ~= "", "snapshot_dir is required")
        local ok, machine = pcall(function()
            return cartesi.machine():load(snapshot_dir, runtime_config)
        end)
        if not ok then
            return nil, tostring(machine)
        end
        return {
            cartesi = cartesi,
            machine = machine,
            reference_block = 0,
        }
    end

    function binding.advance_inputs(instance, inputs, range)
        assert(type(instance) == "table" and instance.machine, "machine instance is required")
        if range.from_block > range.to_block then
            return true
        end
        return advance_inputs_on_machine(instance.machine, instance.cartesi, inputs)
    end

    function binding.inspect_state(instance)
        assert(type(instance) == "table" and instance.machine, "machine instance is required")
        return inspect_on_machine(instance.machine, instance.cartesi)
    end

    function binding.store_snapshot(instance, snapshot_dir)
        assert(type(instance) == "table" and instance.machine, "machine instance is required")
        assert(type(snapshot_dir) == "string" and snapshot_dir ~= "", "snapshot_dir is required")
        local ok, err = pcall(function()
            instance.machine:store(snapshot_dir)
        end)
        if not ok then
            return nil, tostring(err)
        end
        return true
    end

    return machine_runner.new(binding)
end

machine_cartesi._private = {
    STATE_INSPECT_QUERY = STATE_INSPECT_QUERY,
}

return machine_cartesi
