-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local e2e_log = {}

function e2e_log.banner(title)
    io.write("\n[watchdog-e2e] ═══════════════════════════════════════════════════════\n")
    io.write("[watchdog-e2e] SCENARIO: " .. tostring(title) .. "\n")
    io.write("[watchdog-e2e] ═══════════════════════════════════════════════════════\n")
    io.flush()
end

function e2e_log.step(scenario, index, total, message)
    io.write(string.format(
        "[watchdog-e2e] [%s] step %02d/%02d: %s\n",
        tostring(scenario),
        index,
        total,
        tostring(message)
    ))
    io.flush()
end

function e2e_log.info(message)
    io.write("[watchdog-e2e] INFO: " .. tostring(message) .. "\n")
    io.flush()
end

function e2e_log.skip(scenario, reason)
    io.write(string.format("[watchdog-e2e] [%s] SKIP: %s\n", tostring(scenario), tostring(reason)))
    io.flush()
end

function e2e_log.pass(scenario, detail)
    local suffix = detail and (": " .. tostring(detail)) or ""
    io.write(string.format("[watchdog-e2e] [%s] PASS%s\n", tostring(scenario), suffix))
    io.flush()
end

function e2e_log.fail(scenario, err)
    io.write(string.format("[watchdog-e2e] [%s] FAIL: %s\n", tostring(scenario), tostring(err)))
    io.flush()
end

return e2e_log
