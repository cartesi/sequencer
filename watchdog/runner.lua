-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local alarm = require("watchdog.alarm")
local checkpoint = require("watchdog.checkpoint")
local compare = require("watchdog.compare")
local l1 = require("watchdog.l1")

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
    return l1.fetch_inputs(rpc, {
        start_block = from_block,
        end_block = to_block,
        input_box_address = cfg.input_box_address,
        app_address = cfg.app_address,
        input_added_topic = cfg.input_added_topic,
        long_block_range_error_codes = cfg.long_block_range_error_codes,
    })
end

local function send_alarm(cfg, deps, payload)
    if deps.alarm then
        return deps.alarm(payload)
    end
    if deps.http and cfg.webhook_url then
        return alarm.send_webhook(deps.http, cfg.webhook_url, payload)
    end
    return true
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

function runner.run_once(cfg, deps)
    deps = deps or {}
    local checkpoint_mod = deps.checkpoint or checkpoint
    local sequencer = require_dep(deps, "sequencer")
    local machine = require_dep(deps, "machine")

    local loaded = load_checkpoint(cfg, checkpoint_mod)
    local sequencer_state, state_err = sequencer:get_state()
    if not sequencer_state then
        return nil, state_err
    end

    local safe_block_prev = loaded.safe_block or 0
    local safe_block_next = sequencer_state.safe_block
    if safe_block_next < safe_block_prev then
        local payload = {
            kind = "safe_block_regressed",
            previous_safe_block = safe_block_prev,
            sequencer_safe_block = safe_block_next,
        }
        send_alarm(cfg, deps, payload)
        return nil, payload
    end

    local inputs, input_err = fetch_inputs(cfg, deps, safe_block_prev + 1, safe_block_next)
    if not inputs then
        return nil, input_err
    end

    local instance, load_err = machine:load(loaded.snapshot_dir)
    if not instance then
        return nil, load_err
    end

    local fed, feed_err = machine:feed_inputs(instance, inputs)
    if not fed then
        return nil, feed_err
    end

    local cm_state, inspect_err = machine:inspect_state(instance)
    if not cm_state then
        return nil, inspect_err
    end

    local equal, mismatch_offset = compare.raw_equal(sequencer_state.state, cm_state)
    if not equal then
        local payload = {
            kind = "state_mismatch",
            previous_safe_block = safe_block_prev,
            sequencer_safe_block = safe_block_next,
            mismatch_offset = mismatch_offset,
        }
        send_alarm(cfg, deps, payload)
        return nil, payload
    end

    if safe_block_next > safe_block_prev then
        local written, write_err = checkpoint_mod.write(cfg.checkpoint_dir, safe_block_next, function(snapshot_dir)
            return machine:save(instance, snapshot_dir)
        end, {
            created_at = os.date("!%Y-%m-%dT%H:%M:%SZ"),
            cm_image_hash = cfg.cm_image_hash,
        })
        if not written then
            return nil, write_err
        end
    end

    return {
        ok = true,
        previous_safe_block = safe_block_prev,
        safe_block = safe_block_next,
        input_count = #inputs,
    }
end

function runner.advance_checkpoint_once(cfg, deps)
    deps = deps or {}
    local checkpoint_mod = deps.checkpoint or checkpoint
    local machine = require_dep(deps, "machine")

    local loaded = load_checkpoint(cfg, checkpoint_mod)
    local safe_block_prev = loaded.safe_block or 0
    local safe_block_next, safe_err = target_safe_block(cfg, deps)
    if not safe_block_next then
        return nil, safe_err
    end
    if safe_block_next < safe_block_prev then
        return nil, {
            kind = "safe_block_regressed",
            previous_safe_block = safe_block_prev,
            safe_block = safe_block_next,
        }
    end

    local inputs, input_err = fetch_inputs(cfg, deps, safe_block_prev + 1, safe_block_next)
    if not inputs then
        return nil, input_err
    end

    local instance, load_err = machine:load(loaded.snapshot_dir)
    if not instance then
        return nil, load_err
    end

    local fed, feed_err = machine:feed_inputs(instance, inputs)
    if not fed then
        return nil, feed_err
    end

    if safe_block_next > safe_block_prev then
        local written, write_err = checkpoint_mod.write(cfg.checkpoint_dir, safe_block_next, function(snapshot_dir)
            return machine:save(instance, snapshot_dir)
        end, {
            created_at = os.date("!%Y-%m-%dT%H:%M:%SZ"),
            cm_image_hash = cfg.cm_image_hash,
        })
        if not written then
            return nil, write_err
        end
    end

    return {
        ok = true,
        previous_safe_block = safe_block_prev,
        safe_block = safe_block_next,
        input_count = #inputs,
    }
end

return runner
