-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Canonical execution: where it may start, and advancing a checkpoint
--- through the application's L1 inputs up to a block. `init`, `tick`, and
--- `replay` share it.

local errors = require("watchdog.errors")

local canonical = {}

--- A trusted starting point: the stored machine `dir` said to have consumed
--- every input through `block`. Returns `{ dir, block, input_count }`. The
--- count is pinned at a block the RPC holds as safe; when no input precedes
--- the block, the machine must be the application's template (its root hash is
--- the on-chain template hash), so an image from other source or another
--- network cannot start a canonical history.
function canonical.trusted_start(machine, l1, dir, block)
    l1:require_safe_head(block)
    local input_count = l1:input_count_at(block)
    if input_count == 0 and machine.root_hash(dir) ~= l1:template_hash() then
        errors.operator("no input precedes block %d, so the machine at %s must be the application's template, "
            .. "but its root hash differs from the on-chain template hash", block, dir)
    end
    return { dir = dir, block = block, input_count = input_count }
end

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
