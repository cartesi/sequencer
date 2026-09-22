-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- One compare cycle (docs/watchdog/README.md#the-tick).
---
--- The replay target is the block the sequencer's digest describes, fetched
--- before replaying, so a long replay can never chase a moving target. The
--- head only advances past a block the sequencer agreed with; anything else
--- latches a divergence with its evidence.
---
--- Outcomes: `latched` (already latched; no work), `idle` (nothing new),
--- `agreed` (head advanced), `diverged` (newly latched). Failures raise.

local canonical = require("watchdog.canonical")
local errors = require("watchdog.errors")
local incident = require("watchdog.incident")

local tick = {}

--- `deps`: store, machine, sequencer, l1, now().
function tick.run(cfg, deps)
    local store, machine, sequencer = deps.store, deps.machine, deps.sequencer

    local latched = incident.marker(store)
    if latched then
        return { kind = "latched", incident = latched }
    end
    incident.discard_interrupted(store)
    local work = store:reset_work()

    local head = store:head()
    if not head then
        errors.operator("no checkpoint in %s; run init", store.dir)
    end

    local function diverge(event, evidence)
        event.agreed = { block = head.block, input_count = head.input_count }
        evidence.publish = machine.publish
        return { kind = "diverged", incident = incident.latch(store, event, evidence, deps.now()) }
    end

    local function regressed(block)
        return diverge({ kind = "inclusion_block_regressed", target_block = block }, {})
    end

    local polled = sequencer:inclusion_block()
    if polled == head.block then
        return { kind = "idle", head = head }
    elseif polled < head.block then
        return regressed(polled)
    end

    local target = sequencer:digest()
    if target.inclusion_block == head.block then
        return { kind = "idle", head = head }
    elseif target.inclusion_block < head.block then
        return regressed(target.inclusion_block)
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
    for _, old in ipairs(store:checkpoints()) do
        if old.dir ~= published then
            machine.remove(old.dir)
        end
    end
    return {
        kind = "agreed",
        previous = head,
        head = { block = target.inclusion_block, input_count = advanced.input_count, dir = published },
    }
end

return tick
