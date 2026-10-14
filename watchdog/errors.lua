-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Failure classes (docs/watchdog/README.md#commands): `transient`, `operator`,
--- and `internal` for anything else raised, filesystem failures included.
--- Divergences are tick outcomes, never errors.

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
