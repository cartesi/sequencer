-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local state = require("watchdog.state")

local metrics = {}

metrics.STATUS_FILE = "status.prom"
metrics.METRIC_STATUS = "cartesi_watchdog_status"
metrics.METRIC_LAST_TICK = "cartesi_watchdog_last_tick_unix_seconds"
metrics.METRIC_EXIT_CODE = "cartesi_watchdog_exit_code"

local function normalize_env(env)
    if env == nil then
        return os.getenv
    end
    if type(env) == "function" then
        return env
    end
    return function(name)
        return env[name]
    end
end

local function escape_label(value)
    value = tostring(value)
    value = value:gsub("\\", "\\\\")
    value = value:gsub('"', '\\"')
    value = value:gsub("\n", "\\n")
    return value
end

local function base_labels(opts)
    local labels = {}
    local chain = opts.chain_id
    if chain == nil or chain == "" then
        chain = "unknown"
    end
    labels.chain = tostring(chain)

    local app_address = opts.app_address
    if app_address == nil or app_address == "" then
        app_address = "unknown"
    end
    labels.app_address = tostring(app_address)
    return labels
end

local function label_string(labels)
    local keys = {}
    for key in pairs(labels) do
        table.insert(keys, key)
    end
    table.sort(keys)

    local parts = {}
    for _, key in ipairs(keys) do
        table.insert(parts, key .. '="' .. escape_label(labels[key]) .. '"')
    end
    return "{" .. table.concat(parts, ",") .. "}"
end

local function gauge_line(name, labels, value)
    return name .. label_string(labels) .. " " .. tostring(value)
end

function metrics.state_for_exit_code(exit_code)
    if exit_code == 2 then
        return "failed"
    end
    if exit_code == 1 then
        return "warning"
    end
    return "ok"
end

function metrics.resolve_path(cfg, env)
    local getenv = normalize_env(env)
    local configured = getenv("CARTESI_WATCHDOG_METRICS_FILE")
    if configured ~= nil and configured ~= "" then
        return configured
    end
    if cfg and cfg.metrics_file and cfg.metrics_file ~= "" then
        return cfg.metrics_file
    end
    if cfg and cfg.state_dir and cfg.state_dir ~= "" then
        return cfg.state_dir .. "/" .. metrics.STATUS_FILE
    end
    local state_dir = getenv("CARTESI_WATCHDOG_STATE_DIR")
    if state_dir == nil or state_dir == "" then
        error("CARTESI_WATCHDOG_STATE_DIR is required")
    end
    return state_dir .. "/" .. metrics.STATUS_FILE
end

function metrics.build_prom(opts)
    assert(type(opts) == "table", "opts is required")
    assert(type(opts.exit_code) == "number", "exit_code is required")

    local labels = base_labels(opts)
    local active_state = metrics.state_for_exit_code(opts.exit_code)
    local timestamp = opts.timestamp or os.time()

    local lines = {
        "# HELP " .. metrics.METRIC_STATUS .. " Current watchdog compare state (1 = active).",
        "# TYPE " .. metrics.METRIC_STATUS .. " gauge",
        gauge_line(metrics.METRIC_STATUS, {
            chain = labels.chain,
            app_address = labels.app_address,
            state = "ok",
        }, active_state == "ok" and 1 or 0),
        gauge_line(metrics.METRIC_STATUS, {
            chain = labels.chain,
            app_address = labels.app_address,
            state = "warning",
        }, active_state == "warning" and 1 or 0),
        gauge_line(metrics.METRIC_STATUS, {
            chain = labels.chain,
            app_address = labels.app_address,
            state = "failed",
        }, active_state == "failed" and 1 or 0),
        "# HELP " .. metrics.METRIC_LAST_TICK .. " Unix time of the last completed tick.",
        "# TYPE " .. metrics.METRIC_LAST_TICK .. " gauge",
        gauge_line(metrics.METRIC_LAST_TICK, {
            chain = labels.chain,
            app_address = labels.app_address,
        }, timestamp),
        "# HELP " .. metrics.METRIC_EXIT_CODE .. " Process exit code from the last tick.",
        "# TYPE " .. metrics.METRIC_EXIT_CODE .. " gauge",
        gauge_line(metrics.METRIC_EXIT_CODE, {
            chain = labels.chain,
            app_address = labels.app_address,
        }, opts.exit_code),
    }

    if opts.divergence_kind and opts.divergence_kind ~= "" then
        table.insert(lines, "# HELP cartesi_watchdog_divergence_info Divergence kind from the last tick (1 = present).")
        table.insert(lines, "# TYPE cartesi_watchdog_divergence_info gauge")
        table.insert(lines, gauge_line("cartesi_watchdog_divergence_info", {
            chain = labels.chain,
            app_address = labels.app_address,
            kind = opts.divergence_kind,
        }, 1))
    end

    return table.concat(lines, "\n") .. "\n"
end

function metrics.write_tick_status(opts)
    local path = metrics.resolve_path(opts.cfg, opts.env)
    local body = metrics.build_prom(opts)
    return state.write_file_atomic(path, body)
end

return metrics
