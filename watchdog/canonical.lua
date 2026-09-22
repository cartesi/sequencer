-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Canonical execution: advance a checkpoint through the application's L1
--- inputs up to a block. `tick` and `replay` share it.

local canonical = {}

--- Advance `from` ({ dir, block, input_count }) to block `to_block` in
--- `working_dir`. Returns the input count through `to_block`, or, when the
--- machine reaches a fixed point, `stop` = { status, input_index,
--- input_block } for the input that stopped it; nothing after it can run.
function canonical.advance(machine, l1, from, to_block, working_dir)
    l1:require_safe_head(to_block)
    local executor = machine.open(from.dir, working_dir)
    local ok, result = pcall(function()
        local stop
        local count = l1:inputs(from.block + 1, to_block, from.input_count, function(input)
            local status = executor:advance(input.raw)
            if status.kind ~= "accepted" and status.kind ~= "rejected" then
                stop = { status = status, input_index = input.index, input_block = input.block_number }
                return false
            end
        end)
        return { input_count = count, stop = stop }
    end)
    executor:close()
    if not ok then
        error(result, 0)
    end
    return result
end

return canonical
