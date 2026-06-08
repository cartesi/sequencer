-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local compare = {}

function compare.first_mismatch_offset(a, b)
    assert(type(a) == "string", "left value must be a string")
    assert(type(b) == "string", "right value must be a string")

    local limit = math.min(#a, #b)
    for i = 1, limit do
        if a:byte(i) ~= b:byte(i) then
            return i
        end
    end

    if #a ~= #b then
        return limit + 1
    end

    return nil
end

function compare.raw_equal(expected, actual)
    local mismatch = compare.first_mismatch_offset(expected, actual)
    return mismatch == nil, mismatch
end

function compare.assert_state_response(state)
    if type(state) ~= "string" or #state == 0 then
        return nil, "state must be a non-empty string"
    end
    return true
end

return compare
