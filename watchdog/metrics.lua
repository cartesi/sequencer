-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- The Prometheus textfile (`status.prom`) every tick writes before exiting.
--- A textfile-collector sample carries the scrape time, not the tick time, so
--- the file includes its own completion timestamp: a hung or skipped tick is
--- visible as a stale `cartesi_watchdog_last_tick_timestamp_seconds`.

local metrics = {}

local STATES = { "ok", "warning", "failed" }

local function escape(value)
    return (tostring(value):gsub("\\", "\\\\"):gsub('"', '\\"'):gsub("\n", "\\n"))
end

local function labels(base, extra)
    local all = {}
    for key, value in pairs(base) do
        all[key] = value
    end
    for key, value in pairs(extra or {}) do
        all[key] = value
    end
    local keys = {}
    for key in pairs(all) do
        keys[#keys + 1] = key
    end
    table.sort(keys)
    local parts = {}
    for _, key in ipairs(keys) do
        parts[#parts + 1] = string.format('%s="%s"', key, escape(all[key]))
    end
    return "{" .. table.concat(parts, ",") .. "}"
end

function metrics.state_for_exit_code(exit_code)
    return exit_code == 2 and "failed" or exit_code == 1 and "warning" or "ok"
end

--- `report`: chain_id and app_address (nil when the config could not be
--- read), exit_code, timestamp (unix seconds), and optionally head_block and
--- divergence_kind (while latched).
function metrics.render(report)
    local base = {
        chain = report.chain_id and tostring(report.chain_id) or "unknown",
        app_address = report.app_address or "unknown",
    }
    local state = metrics.state_for_exit_code(report.exit_code)
    local lines = {
        "# HELP cartesi_watchdog_status Outcome of the last tick (1 = current state).",
        "# TYPE cartesi_watchdog_status gauge",
    }
    local function gauge(name, extra, value)
        lines[#lines + 1] = name .. labels(base, extra) .. " " .. tostring(value)
    end
    for _, name in ipairs(STATES) do
        gauge("cartesi_watchdog_status", { state = name }, name == state and 1 or 0)
    end
    if report.divergence_kind then
        lines[#lines + 1] = "# HELP cartesi_watchdog_divergence_info Latched divergence kind (1 = latched)."
        lines[#lines + 1] = "# TYPE cartesi_watchdog_divergence_info gauge"
        gauge("cartesi_watchdog_divergence_info", { kind = report.divergence_kind }, 1)
    end
    if report.head_block then
        lines[#lines + 1] = "# HELP cartesi_watchdog_head_block L1 block of the watchdog's head checkpoint."
        lines[#lines + 1] = "# TYPE cartesi_watchdog_head_block gauge"
        gauge("cartesi_watchdog_head_block", nil, report.head_block)
    end
    lines[#lines + 1] = "# HELP cartesi_watchdog_last_tick_timestamp_seconds When the last tick finished."
    lines[#lines + 1] = "# TYPE cartesi_watchdog_last_tick_timestamp_seconds gauge"
    gauge("cartesi_watchdog_last_tick_timestamp_seconds", nil, report.timestamp)
    return table.concat(lines, "\n") .. "\n"
end

return metrics
