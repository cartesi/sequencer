-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- `replay`: re-derive canonical state from a trusted stored machine into a
--- new directory, with the same executor and L1 reader as `tick`, and report
--- its digest. It never touches the state directory. Incident triage uses it
--- to check whether the canonical side is reproducible, and to produce a
--- canonical checkpoint at a chosen block for recovery.

local canonical = require("watchdog.canonical")
local errors = require("watchdog.errors")

local replay = {}

--- `args`: from_dir, from_block, to_block, out_dir. `deps`: machine, l1.
function replay.run(cfg, deps, args)
    local machine, l1 = deps.machine, deps.l1
    if args.to_block < args.from_block then
        errors.operator("--to-block %d precedes --from-block %d", args.to_block, args.from_block)
    end
    if l1:chain_id() ~= cfg.chain_id then
        errors.operator("the L1 RPC does not serve chain %d", cfg.chain_id)
    end
    local from = {
        dir = args.from_dir,
        block = args.from_block,
        input_count = l1:input_count_at(args.from_block),
    }
    local advanced = canonical.advance(machine, l1, from, args.to_block, args.out_dir)
    machine.sync(args.out_dir)
    local result = {
        from_block = args.from_block,
        to_block = args.to_block,
        out_dir = args.out_dir,
    }
    if advanced.stop then
        result.stop = {
            status = advanced.stop.status.kind,
            description = machine.describe(advanced.stop.status),
            input_index = advanced.stop.input_index,
            input_block = advanced.stop.input_block,
        }
    else
        result.input_count = advanced.input_count
        result.sha256 = machine.sha256(machine.state_bytes(args.out_dir, cfg.state_source))
    end
    return result
end

return replay
