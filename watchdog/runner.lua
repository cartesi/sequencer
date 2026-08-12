-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local checkpoint = require("watchdog.checkpoint")
local compare = require("watchdog.compare")
local l1_reader = require("watchdog.l1_reader")

local runner = {}

local function require_dep(deps, name)
    local value = deps[name]
    assert(value ~= nil, "missing dependency: " .. name)
    return value
end

local function load_checkpoint(cfg, checkpoint_mod)
    local loaded, load_err = checkpoint_mod.load(cfg.state_dir)
    if loaded then
        return loaded
    end
    -- Operator/state error, not a transient RPC blip: tick never bootstraps a
    -- head from env. Point at init so the failure is actionable in logs.
    error(
        "failed to load watchdog head: "
            .. tostring(load_err)
            .. "; run `sequencer-watchdog init` (wipe state_dir and re-init if state is incomplete)"
    )
end

local function ensure_rpc_head_covers_target(deps, target_block)
    if deps.fetch_inputs or deps.for_each_input_chunk then
        return true
    end

    local rpc = deps.rpc
    if not rpc or type(rpc.get_block_number_by_tag) ~= "function" then
        return true
    end

    local head, err = l1_reader.ensure_rpc_head_at_least(rpc, target_block)
    if not head then
        return nil, err
    end
    return true
end

local function l1_params(cfg, from_block, to_block)
    return {
        start_block = from_block,
        end_block = to_block,
        input_box_address = cfg.input_box_address,
        app_address = cfg.app_address,
        input_added_topic = cfg.input_added_topic,
        long_block_range_error_codes = cfg.long_block_range_error_codes,
    }
end

local function for_each_input_chunk(cfg, deps, from_block, to_block, on_chunk)
    if from_block > to_block then
        return 0
    end

    if deps.for_each_input_chunk then
        return deps.for_each_input_chunk(from_block, to_block, on_chunk)
    end

    if deps.fetch_inputs then
        local inputs, err = deps.fetch_inputs(from_block, to_block)
        if not inputs then
            return nil, err
        end
        local ok, callback_err = on_chunk(inputs, {
            from_block = from_block,
            to_block = to_block,
        })
        if not ok then
            return nil, callback_err
        end
        return #inputs
    end

    local rpc = require_dep(deps, "rpc")
    return l1_reader.for_each_input_chunk_partitioned(rpc, l1_params(cfg, from_block, to_block), on_chunk)
end

local function step(deps, message)
    if deps and type(deps.log_step) == "function" then
        deps.log_step(message)
    end
end

local function compare_and_checkpoint(cfg, deps, instance, safe_block_prev, safe_block_next, input_count)
    local checkpoint_mod = deps.checkpoint or checkpoint
    local machine = require_dep(deps, "machine")

    step(deps, "run CM inspect (state query)")
    local cm_state, inspect_err = machine:inspect(instance)
    if not cm_state then
        return nil, inspect_err
    end

    local sequencer = require_dep(deps, "sequencer")
    step(deps, "fetch sequencer GET /finalized_state")
    local finalized, state_err = sequencer:get_finalized_state()
    if not finalized then
        return nil, state_err
    end
    if finalized.not_modified then
        return nil, "finalized state unexpectedly returned 304 during compare"
    end
    if finalized.inclusion_block ~= safe_block_next then
        return nil, string.format(
            "finalized inclusion_block moved during compare cycle (%s -> %s); retry",
            tostring(safe_block_next),
            tostring(finalized.inclusion_block)
        )
    end

    step(deps, "compare finalized SSZ bytes against CM inspect report")
    local equal, mismatch_offset = compare.raw_equal(finalized.state, cm_state)
    if not equal then
        return nil, {
            kind = "state_mismatch",
            previous_safe_block = safe_block_prev,
            sequencer_inclusion_block = finalized.inclusion_block,
            mismatch_offset = mismatch_offset,
        }
    end

    -- Compare succeeded: persist a fresh block-named checkpoint before flipping
    -- head.json. The previous checkpoint is pruned after the pointer flip.
    step(deps, "persist new manifest-backed checkpoint")
    local written, write_err = checkpoint_mod.write(cfg.state_dir, safe_block_next, function(snapshot_dir)
        return machine:dump(instance, snapshot_dir, safe_block_next)
    end, {
        created_at = os.date("!%Y-%m-%dT%H:%M:%SZ"),
        cm_image_hash = cfg.cm_image_hash,
    })
    if not written then
        return nil, write_err
    end

    return {
        ok = true,
        previous_safe_block = safe_block_prev,
        safe_block = safe_block_next,
        input_count = input_count,
    }
end

--- L1 fetch → CM advance → inspect → compare → checkpoint.
local function run_pass(cfg, deps, loaded, safe_block_prev, safe_block_next)
    step(deps, string.format("check L1 RPC head covers target block %d", safe_block_next))
    local head_ok, head_err = ensure_rpc_head_covers_target(deps, safe_block_next)
    if not head_ok then
        return nil, head_err
    end

    local machine = require_dep(deps, "machine")
    step(deps, "load CM snapshot directory")
    local instance, load_err = machine:load(loaded.snapshot_dir, safe_block_prev)
    if not instance then
        return nil, load_err
    end

    step(deps, string.format(
        "stream L1 InputAdded logs for blocks %s..%s",
        tostring(safe_block_prev + 1),
        tostring(safe_block_next)
    ))
    local input_count = 0
    local advanced_count, input_err = for_each_input_chunk(
        cfg,
        deps,
        safe_block_prev + 1,
        safe_block_next,
        function(inputs, range)
            input_count = input_count + #inputs
            step(deps, string.format(
                "feed %d decoded inputs into CM for blocks %d..%d",
                #inputs,
                range.from_block,
                range.to_block
            ))
            return machine:advance(instance, inputs, range)
        end
    )
    if not advanced_count then
        return nil, input_err
    end

    return compare_and_checkpoint(cfg, deps, instance, safe_block_prev, safe_block_next, input_count)
end

local function skip_result(safe_block_prev, safe_block_next, reason)
    return {
        ok = true,
        skipped = true,
        skip_reason = reason,
        previous_safe_block = safe_block_prev,
        safe_block = safe_block_next,
        input_count = 0,
    }
end

function runner.run_once(cfg, deps)
    deps = deps or {}
    local checkpoint_mod = deps.checkpoint or checkpoint

    step(deps, "load watchdog checkpoint")
    local loaded = load_checkpoint(cfg, checkpoint_mod)

    local safe_block_prev = loaded.safe_block or 0
    local sequencer = require_dep(deps, "sequencer")
    step(deps, "fetch sequencer GET /finalized_state/inclusion_block")
    local head, head_err = sequencer:get_finalized_inclusion_block()
    if not head then
        return nil, head_err
    end

    local safe_block_next = head.inclusion_block
    step(deps, string.format(
        "check inclusion_block monotonicity (prev=%s next=%s)",
        tostring(safe_block_prev),
        tostring(safe_block_next)
    ))
    if safe_block_next < safe_block_prev then
        return nil, {
            kind = "inclusion_block_regressed",
            previous_safe_block = safe_block_prev,
            sequencer_inclusion_block = safe_block_next,
        }
    end

    if safe_block_next == safe_block_prev then
        step(deps, "finalized inclusion_block unchanged; skip L1/CM/compare cycle")
        return skip_result(safe_block_prev, safe_block_next, "finalized_unchanged")
    end

    local result, err = run_pass(cfg, deps, loaded, safe_block_prev, safe_block_next)
    if result then
        step(deps, "compare pass complete")
    end
    return result, err
end

return runner
