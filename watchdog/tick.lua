-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- One compare cycle (docs/watchdog/README.md#the-tick).

local canonical = require("watchdog.canonical")
local errors = require("watchdog.errors")
local incident = require("watchdog.incident")
local store_mod = require("watchdog.store")

local tick = {}

--- `deps`: store, machine, sequencer, l1, now(). Returns `{ kind }`: `latched`
--- (no work), `idle`, `agreed` (head advanced), or `diverged`, latched before
--- anything else and reported even with `latch_error` (`incident/` not created)
--- or `record_error` (only its record not written); its `evidence` is for the
--- caller to collect after it has signalled. Failures raise.
function tick.run(cfg, deps)
    local store, machine, sequencer = deps.store, deps.machine, deps.sequencer

    local latched = incident.marker(store)
    if latched then
        return { kind = "latched", incident = latched }
    end
    local work = store:reset_work()

    local head = store:head()
    if not head then
        errors.operator("no checkpoint in %s; run init", store.dir)
    end

    local function diverge(event, evidence)
        event.agreed = { block = head.block, input_count = head.input_count }
        local ok, record_error = pcall(incident.latch, store, event, deps.now())
        if not ok then
            return { kind = "diverged", incident = event, latch_error = select(2, errors.classify(record_error)) }
        end
        if evidence then
            evidence.publish = machine.publish
        end
        return { kind = "diverged", incident = event, record_error = record_error, evidence = evidence }
    end

    --- Nothing to compare at `block`, or a regression. Only an agreed block can
    --- regress; a sequencer behind the never-agreed bootstrap head has not reached it.
    local function behind(block)
        if block == head.block then
            return { kind = "idle", head = head }
        elseif head.block == cfg.bootstrap_block then
            return { kind = "idle", head = head, sequencer_block = block }
        end
        return diverge({ kind = "inclusion_block_regressed", target_block = block })
    end

    local polled = sequencer:inclusion_block()
    if polled <= head.block then
        return behind(polled)
    end
    -- The target is fixed before replaying, so a long replay never chases a moving one.
    local target = sequencer:digest()
    if target.inclusion_block <= head.block then
        return behind(target.inclusion_block)
    end

    if deps.l1:chain_id() ~= cfg.chain_id then
        errors.operator("the L1 RPC does not serve chain %d", cfg.chain_id)
    end

    local working = work .. "/working"
    local advanced = canonical.advance(machine, deps.l1, head, target.inclusion_block, working)
    if advanced.stop then
        return diverge({
            kind = "canonical_machine_dead",
            target_block = target.inclusion_block,
            stop = {
                status = advanced.stop.status.kind,
                description = machine.describe(advanced.stop.status),
                input_index = advanced.stop.input_index,
                input_block = advanced.stop.input_block,
            },
        }, { machine = working })
    end

    local bytes = machine.state_bytes(working, cfg.state_source)
    local digest = machine.sha256(bytes)
    if digest ~= target.sha256 then
        return diverge({
            kind = "state_mismatch",
            target_block = target.inclusion_block,
            canonical_sha256 = digest,
            sequencer_sha256 = target.sha256,
        }, { machine = working, canonical_bytes = bytes, sequencer = sequencer })
    end

    local published = store:checkpoint_dir(target.inclusion_block, advanced.input_count)
    machine.publish(working, published)
    -- A plain tree removal: an interrupted one is finished by the next prune.
    for _, old in ipairs(store:checkpoints()) do
        if old.dir ~= published then
            store_mod.remove_tree(old.dir)
        end
    end
    return {
        kind = "agreed",
        previous = head,
        head = { block = target.inclusion_block, input_count = advanced.input_count, dir = published },
    }
end

return tick
