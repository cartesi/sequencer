-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local config = {}

config.VERSION = 1

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

local function normalize_env(env)
    env = env or os.getenv
    if type(env) == "function" then
        local getenv = env
        env = setmetatable({}, {
            __index = function(_, key)
                return getenv(key)
            end,
        })
    end
    return env
end

function config.load_state_dir(env)
    env = normalize_env(env)
    return required("CARTESI_WATCHDOG_STATE_DIR", env)
end

function config.load_init(env)
    env = normalize_env(env)

    -- The watchdog has one job: compare the sequencer's finalized state against
    -- a canonical CM re-derivation. One cycle per process; infra schedules it.
    return {
        version = config.VERSION,
        sequencer_url = required("CARTESI_WATCHDOG_SEQUENCER_URL", env),
        input_box_address = required("CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS", env),
        app_address = required("CARTESI_WATCHDOG_APP_ADDRESS", env),
        input_added_topic = env.CARTESI_WATCHDOG_INPUT_ADDED_TOPIC,
        state_dir = required("CARTESI_WATCHDOG_STATE_DIR", env),
        cm_snapshot_dir = required("CARTESI_WATCHDOG_CM_SNAPSHOT_DIR", env),
        cm_snapshot_safe_block = optional_required_number(
            "CARTESI_WATCHDOG_CM_SNAPSHOT_DIR",
            "CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK",
            env
        ),
        cm_image_hash = env.CARTESI_WATCHDOG_CM_IMAGE_HASH,
        blockchain_id = env.CARTESI_WATCHDOG_BLOCKCHAIN_ID,
        metrics_file = env.CARTESI_WATCHDOG_METRICS_FILE,
        retry_attempts = optional_number("CARTESI_WATCHDOG_RETRY_ATTEMPTS", 3, env),
        retry_delay_sec = optional_number("CARTESI_WATCHDOG_RETRY_DELAY_SEC", 5, env),
        long_block_range_error_codes = split_csv(
            env.CARTESI_WATCHDOG_LONG_BLOCK_RANGE_ERROR_CODES or "-32005,-32012,-32600,-32602,-32616"
        ),
    }
end

local function required_field(data, name)
    local value = data[name]
    if value == nil or value == "" then
        error("config.json missing " .. name)
    end
    return value
end

local function optional_field_number(data, name, default)
    local value = data[name]
    if value == nil then
        return default
    end
    if type(value) ~= "number" then
        error("config.json " .. name .. " must be a number")
    end
    return value
end

function config.persisted(cfg)
    return {
        version = config.VERSION,
        sequencer_url = cfg.sequencer_url,
        input_box_address = cfg.input_box_address,
        app_address = cfg.app_address,
        input_added_topic = cfg.input_added_topic,
        cm_image_hash = cfg.cm_image_hash,
        blockchain_id = cfg.blockchain_id,
        retry_attempts = cfg.retry_attempts,
        retry_delay_sec = cfg.retry_delay_sec,
        long_block_range_error_codes = cfg.long_block_range_error_codes,
    }
end

function config.from_persisted(state_dir, data, env)
    env = normalize_env(env)
    if data.version ~= config.VERSION then
        error("unsupported config.json version: " .. tostring(data.version))
    end
    return {
        version = config.VERSION,
        state_dir = state_dir,
        sequencer_url = (env.CARTESI_WATCHDOG_SEQUENCER_URL ~= nil and env.CARTESI_WATCHDOG_SEQUENCER_URL ~= "")
            and env.CARTESI_WATCHDOG_SEQUENCER_URL
            or required_field(data, "sequencer_url"),
        l1_rpc_url = required("CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT", env),
        input_box_address = required_field(data, "input_box_address"),
        app_address = required_field(data, "app_address"),
        input_added_topic = data.input_added_topic,
        cm_image_hash = data.cm_image_hash,
        blockchain_id = (env.CARTESI_WATCHDOG_BLOCKCHAIN_ID ~= nil and env.CARTESI_WATCHDOG_BLOCKCHAIN_ID ~= "")
            and env.CARTESI_WATCHDOG_BLOCKCHAIN_ID
            or data.blockchain_id,
        metrics_file = env.CARTESI_WATCHDOG_METRICS_FILE,
        retry_attempts = optional_field_number(data, "retry_attempts", 3),
        retry_delay_sec = optional_field_number(data, "retry_delay_sec", 5),
        long_block_range_error_codes = data.long_block_range_error_codes or {},
    }
end

config.load = config.load_init

return config
