-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Watchdog configuration. `init` reads the environment once and persists the
--- deployment identity and the state source in `config.json`; every later
--- command reads that file. Only the two endpoints, which operators rotate,
--- come from the environment at run time.

local errors = require("watchdog.errors")
local l1 = require("watchdog.l1")

local config = {}

config.VERSION = 2

local function getter(env)
    if type(env) == "table" then
        return function(name)
            return env[name]
        end
    end
    return env or os.getenv
end

local function optional(get, name)
    local value = get(name)
    if value == nil or value == "" then
        return nil
    end
    return value
end

local function required(get, name)
    return optional(get, name) or errors.operator("%s is required", name)
end

local function absolute_path(value, name)
    if value:sub(1, 1) ~= "/" then
        errors.operator("%s must be an absolute path, got %q", name, value)
    end
    return (value:gsub("/+$", ""))
end

local function block_number(value, name)
    local number = math.tointeger(tonumber(value))
    if not number or number < 0 then
        errors.operator("%s must be a non-negative integer, got %q", name, tostring(value))
    end
    return number
end

--- `0x` + lowercase hex; checksum casing must not matter.
function config.normalize_address(value, name)
    local raw = tostring(value):gsub("^0[xX]", ""):lower()
    if #raw ~= 40 or raw:match("^[0-9a-f]+$") == nil then
        errors.operator("%s must be a 20-byte hex address, got %q", name, tostring(value))
    end
    return "0x" .. raw
end

--- `inspect`, or `range:<label>` for a whole labeled flash drive or NVRAM.
function config.parse_state_source(value)
    if value == "inspect" then
        return { kind = "inspect" }
    end
    local label = type(value) == "string" and value:match("^range:([a-z][a-z0-9-]*)$")
    if label then
        return { kind = "range", label = label }
    end
    errors.operator("CARTESI_WATCHDOG_STATE_SOURCE must be `inspect` or `range:<label>`, got %q", tostring(value))
end

function config.format_state_source(source)
    return source.kind == "range" and ("range:" .. source.label) or source.kind
end

local function error_codes(value)
    if value == nil then
        return l1.DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES
    end
    local codes = {}
    for code in value:gmatch("[^,%s]+") do
        codes[#codes + 1] = code
    end
    if #codes == 0 then
        errors.operator("CARTESI_WATCHDOG_LONG_BLOCK_RANGE_ERROR_CODES lists no codes")
    end
    return codes
end

--- Everything `init` needs. `chain_id` is nil unless pinned in the
--- environment; init then takes it from the RPC.
function config.from_init_env(env)
    local get = getter(env)
    local chain_id = optional(get, "CARTESI_WATCHDOG_BLOCKCHAIN_ID")
    return {
        state_dir = absolute_path(required(get, "CARTESI_WATCHDOG_STATE_DIR"), "CARTESI_WATCHDOG_STATE_DIR"),
        sequencer_url = required(get, "CARTESI_WATCHDOG_SEQUENCER_URL"),
        rpc_url = required(get, "CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT"),
        chain_id = chain_id and block_number(chain_id, "CARTESI_WATCHDOG_BLOCKCHAIN_ID"),
        input_box_address = config.normalize_address(
            required(get, "CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS"),
            "CARTESI_WATCHDOG_CONTRACTS_INPUT_BOX_ADDRESS"
        ),
        app_address = config.normalize_address(
            required(get, "CARTESI_WATCHDOG_APP_ADDRESS"),
            "CARTESI_WATCHDOG_APP_ADDRESS"
        ),
        state_source = config.parse_state_source(required(get, "CARTESI_WATCHDOG_STATE_SOURCE")),
        long_block_range_error_codes = error_codes(optional(get, "CARTESI_WATCHDOG_LONG_BLOCK_RANGE_ERROR_CODES")),
        bootstrap_dir = absolute_path(
            required(get, "CARTESI_WATCHDOG_CM_SNAPSHOT_DIR"),
            "CARTESI_WATCHDOG_CM_SNAPSHOT_DIR"
        ),
        bootstrap_block = block_number(
            required(get, "CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK"),
            "CARTESI_WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK"
        ),
    }
end

--- The `config.json` document.
function config.persisted(cfg)
    return {
        version = config.VERSION,
        sequencer_url = cfg.sequencer_url,
        chain_id = cfg.chain_id,
        input_box_address = cfg.input_box_address,
        app_address = cfg.app_address,
        state_source = config.format_state_source(cfg.state_source),
        long_block_range_error_codes = cfg.long_block_range_error_codes,
    }
end

--- Whether a persisted document describes the same deployment and source.
function config.same_identity(a, b)
    return a.chain_id == b.chain_id
        and a.input_box_address == b.input_box_address
        and a.app_address == b.app_address
        and a.state_source == b.state_source
end

--- The run-time configuration from a persisted document plus the endpoint
--- environment. `rpc_url` stays nil when unset; commands that read L1 require it.
function config.load(state_dir, document, env)
    local get = getter(env)
    if type(document) ~= "table" or document.version ~= config.VERSION then
        errors.operator("%s/config.json is not a version %d watchdog config; wipe the state directory and re-run init",
            state_dir, config.VERSION)
    end
    local codes = document.long_block_range_error_codes
    if type(codes) ~= "table" or #codes == 0 then
        errors.operator("config.json lists no long_block_range_error_codes")
    end
    return {
        state_dir = state_dir,
        sequencer_url = optional(get, "CARTESI_WATCHDOG_SEQUENCER_URL") or document.sequencer_url,
        rpc_url = optional(get, "CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT"),
        chain_id = block_number(document.chain_id, "config.json chain_id"),
        input_box_address = config.normalize_address(document.input_box_address, "config.json input_box_address"),
        app_address = config.normalize_address(document.app_address, "config.json app_address"),
        state_source = config.parse_state_source(document.state_source),
        long_block_range_error_codes = codes,
    }
end

--- Where `tick` writes `status.prom`, when not in the state directory.
function config.metrics_file(env)
    return optional(getter(env), "CARTESI_WATCHDOG_METRICS_FILE")
end

--- The state directory, the one setting every command shares.
function config.state_dir(env)
    local get = getter(env)
    return absolute_path(required(get, "CARTESI_WATCHDOG_STATE_DIR"), "CARTESI_WATCHDOG_STATE_DIR")
end

return config
