-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local config = {}

local function required(name, env)
    local value = env[name]
    if value == nil or value == "" then
        error(name .. " is required")
    end
    return value
end

local function optional_number(name, default, env)
    local value = env[name]
    if value == nil or value == "" then
        return default
    end
    local parsed = tonumber(value)
    if not parsed then
        error(name .. " must be a number")
    end
    return parsed
end

local function optional_required_number(value_name, number_name, env)
    if env[value_name] == nil or env[value_name] == "" then
        return nil
    end
    local value = env[number_name]
    if value == nil or value == "" then
        error(number_name .. " is required when " .. value_name .. " is set")
    end
    return optional_number(number_name, nil, env)
end

local function split_csv(value)
    local out = {}
    for part in tostring(value or ""):gmatch("[^,]+") do
        table.insert(out, part)
    end
    return out
end

function config.load(env)
    env = env or os.getenv
    if type(env) == "function" then
        local getenv = env
        env = setmetatable({}, {
            __index = function(_, key)
                return getenv(key)
            end,
        })
    end

    local mode = env.WATCHDOG_MODE or "advance"
    if mode ~= "advance" and mode ~= "compare" then
        error("WATCHDOG_MODE must be 'advance' or 'compare'")
    end

    return {
        mode = mode,
        sequencer_url = mode == "compare" and required("WATCHDOG_SEQUENCER_URL", env) or env.WATCHDOG_SEQUENCER_URL,
        l1_rpc_url = required("WATCHDOG_L1_RPC_URL", env),
        input_box_address = required("WATCHDOG_INPUTBOX_ADDRESS", env),
        app_address = required("WATCHDOG_APP_ADDRESS", env),
        input_added_topic = env.WATCHDOG_INPUT_ADDED_TOPIC,
        checkpoint_dir = required("WATCHDOG_CHECKPOINT_DIR", env),
        cm_snapshot_dir = env.WATCHDOG_CM_SNAPSHOT_DIR,
        cm_snapshot_safe_block = optional_required_number(
            "WATCHDOG_CM_SNAPSHOT_DIR",
            "WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK",
            env
        ),
        cm_work_dir = env.WATCHDOG_CM_WORK_DIR or "/tmp",
        cm_executable = env.WATCHDOG_CM_EXECUTABLE or "cartesi-machine",
        target_safe_block = optional_number("WATCHDOG_TARGET_SAFE_BLOCK", nil, env),
        poll_interval_sec = optional_number("WATCHDOG_POLL_INTERVAL_SEC", 30, env),
        retry_attempts = optional_number("WATCHDOG_RETRY_ATTEMPTS", 3, env),
        retry_delay_sec = optional_number("WATCHDOG_RETRY_DELAY_SEC", 5, env),
        safe_confirmations = optional_number("WATCHDOG_SAFE_CONFIRMATIONS", 12, env),
        webhook_url = env.WATCHDOG_WEBHOOK_URL,
        once = env.WATCHDOG_ONCE == "1",
        long_block_range_error_codes = split_csv(
            env.WATCHDOG_LONG_BLOCK_RANGE_ERROR_CODES or "-32005,-32600,-32602,-32616"
        ),
    }
end

return config
