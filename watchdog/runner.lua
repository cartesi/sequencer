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
    local loaded = checkpoint_mod.load(cfg.checkpoint_dir)
    if loaded then
        return loaded
    end

    if not cfg.cm_snapshot_dir or cfg.cm_snapshot_dir == "" then
        error("no checkpoint found and WATCHDOG_CM_SNAPSHOT_DIR is not configured")
    end
    if type(cfg.cm_snapshot_safe_block) ~= "number" then
        error("no checkpoint found and WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK is not configured")
    end

    return {
        snapshot_dir = cfg.cm_snapshot_dir,
        safe_block = cfg.cm_snapshot_safe_block,
    }
end

local function fetch_inputs(cfg, deps, from_block, to_block)
    if from_block > to_block then
        return {}
    end

    if deps.fetch_inputs then
        return deps.fetch_inputs(from_block, to_block)
    end

    local rpc = require_dep(deps, "rpc")
    return l1_reader.fetch_inputs(rpc, {
        start_block = from_block,
        end_block = to_block,
        input_box_address = cfg.input_box_address,
        app_address = cfg.app_address,
        input_added_topic = cfg.input_added_topic,
        long_block_range_error_codes = cfg.long_block_range_error_codes,
    })
end

local function step(deps, message)
    if deps and type(deps.log_step) == "function" then
        deps.log_step(message)
    end
end

local function target_safe_block(cfg, deps)
    if type(cfg.target_safe_block) == "number" then
        return cfg.target_safe_block
    end
    if deps.safe_block then
        return deps.safe_block()
    end
    if deps.rpc and type(deps.rpc.get_block_number_by_tag) == "function" then
        return deps.rpc:get_block_number_by_tag("safe")
    end
    return nil, "target safe block is not configured"
end

local function advance_compare_and_checkpoint(
    cfg,
    deps,
    loaded,
    inputs,
    safe_block_prev,
    safe_block_next,
    with_compare
)
    local checkpoint_mod = deps.checkpoint or checkpoint
    local machine = require_dep(deps, "machine")

    step(deps, "load CM snapshot directory")
    local instance, load_err = machine:load(loaded.snapshot_dir, safe_block_prev)
    if not instance then
        return nil, load_err
    end

    if safe_block_next > safe_block_prev then
        step(deps, string.format("advance CM for blocks %d..%d", safe_block_prev + 1, safe_block_next))
        local advanced, advance_err = machine:advance(instance, inputs, {
            from_block = safe_block_prev + 1,
            to_block = safe_block_next,
        })
        if not advanced then
            return nil, advance_err
        end
    else
        instance.reference_block = safe_block_prev
    end

    if with_compare then
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
    end

    if safe_block_next > safe_block_prev then
        step(deps, "persist new manifest-backed checkpoint")
        local written, write_err = checkpoint_mod.write(cfg.checkpoint_dir, safe_block_next, function(snapshot_dir)
            return machine:dump(instance, snapshot_dir, safe_block_next)
        end, {
            created_at = os.date("!%Y-%m-%dT%H:%M:%SZ"),
            cm_image_hash = cfg.cm_image_hash,
        })
        if not written then
            return nil, write_err
        end
    else
        step(deps, "reference block unchanged; skip checkpoint write")
    end

    return {
        ok = true,
        previous_safe_block = safe_block_prev,
        safe_block = safe_block_next,
        input_count = #inputs,
    }
end

--- Shared path: L1 fetch → CM advance/inspect/compare → optional checkpoint write.
local function run_pass(cfg, deps, loaded, safe_block_prev, safe_block_next, with_compare)
    step(deps, string.format(
        "fetch L1 InputAdded logs for blocks %s..%s",
        tostring(safe_block_prev + 1),
        tostring(safe_block_next)
    ))
    local inputs, input_err = fetch_inputs(cfg, deps, safe_block_prev + 1, safe_block_next)
    if not inputs then
        return nil, input_err
    end

    step(deps, string.format("feed %d decoded inputs into CM", #inputs))
    return advance_compare_and_checkpoint(
        cfg,
        deps,
        loaded,
        inputs,
        safe_block_prev,
        safe_block_next,
        with_compare
    )
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

    step(deps, "load checkpoint or bootstrap CM snapshot")
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

    if safe_block_next <= safe_block_prev then
        step(deps, "finalized inclusion_block unchanged; skip L1/CM/compare cycle")
        return skip_result(safe_block_prev, safe_block_next, "finalized_unchanged")
    end

    local result, err = run_pass(cfg, deps, loaded, safe_block_prev, safe_block_next, true)
    if result then
        step(deps, "compare pass complete")
    end
    return result, err
end

function runner.advance_checkpoint_once(cfg, deps)
    deps = deps or {}
    local checkpoint_mod = deps.checkpoint or checkpoint

    step(deps, "load checkpoint or bootstrap CM snapshot")
    local loaded = load_checkpoint(cfg, checkpoint_mod)
    local safe_block_prev = loaded.safe_block or 0

    step(deps, "resolve target safe block")
    local safe_block_next, safe_err = target_safe_block(cfg, deps)
    if not safe_block_next then
        return nil, safe_err
    end

    step(deps, string.format(
        "check safe_block monotonicity (prev=%s next=%s)",
        tostring(safe_block_prev),
        tostring(safe_block_next)
    ))
    if safe_block_next < safe_block_prev then
        return nil, {
            kind = "safe_block_regressed",
            previous_safe_block = safe_block_prev,
            safe_block = safe_block_next,
        }
    end

    if safe_block_next <= safe_block_prev then
        step(deps, "L1 safe block unchanged; skip advance cycle")
        return skip_result(safe_block_prev, safe_block_next, "safe_unchanged")
    end

    local result, err = run_pass(cfg, deps, loaded, safe_block_prev, safe_block_next, false)
    if result then
        step(deps, "advance pass complete")
    end
    return result, err
end

return runner
