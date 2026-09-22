-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Failure classes. Expected failures are raised as tables with a class;
--- anything else raised (a failed assertion, a Lua error) is an internal
--- fault. Divergences are tick outcomes, never errors.
---
--- - transient: the environment (L1 RPC, sequencer HTTP, I/O) may recover by
---   the next scheduled tick.
--- - operator: configuration or state that only an operator can fix.

local errors = {}

local Error = {}
Error.__index = Error
Error.__tostring = function(err)
    return err.class .. ": " .. err.message
end

local function raise(class, fmt, ...)
    local message = select("#", ...) > 0 and string.format(fmt, ...) or fmt
    error(setmetatable({ class = class, message = message }, Error), 0)
end

function errors.transient(fmt, ...)
    raise("transient", fmt, ...)
end

function errors.operator(fmt, ...)
    raise("operator", fmt, ...)
end

--- Class and message of anything caught by `pcall`.
function errors.classify(err)
    if getmetatable(err) == Error then
        return err.class, err.message
    end
    return "internal", tostring(err)
end

return errors
