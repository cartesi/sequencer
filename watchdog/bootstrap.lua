-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- `init`: store a trusted canonical machine as the first checkpoint and
--- persist the configuration.
---
--- The bootstrap machine must wait for an input and yield the configured
--- state source. When no input precedes its block, it must also be the
--- application's template: its root hash must equal the on-chain template
--- hash. Init is idempotent on a complete state directory and refuses one
--- initialized for a different deployment or state source.

local config = require("watchdog.config")
local errors = require("watchdog.errors")

local bootstrap = {}

--- `deps`: store, machine, l1.
function bootstrap.run(cfg, deps)
    local store, machine, l1 = deps.store, deps.machine, deps.l1

    local existing = store:read_json("config.json")
    if existing then
        local wanted = config.persisted(cfg)
        wanted.chain_id = cfg.chain_id or existing.chain_id
        if not config.same_identity(existing, wanted) then
            errors.operator("%s was initialized for another deployment or state source; "
                .. "wipe it to re-initialize", store.dir)
        end
        local head = store:head()
        if not head then
            errors.operator("%s has config.json but no checkpoint; wipe it and re-run init", store.dir)
        end
        return { kind = "already_initialized", head = head }
    end

    -- No config.json: nothing here is initialized, so any checkpoint is an
    -- interrupted init's.
    store:remove("checkpoints")
    local work = store:reset_work()

    local rpc_chain_id = l1:chain_id()
    if cfg.chain_id and cfg.chain_id ~= rpc_chain_id then
        errors.operator("CARTESI_WATCHDOG_BLOCKCHAIN_ID is %d but the L1 RPC serves chain %d",
            cfg.chain_id, rpc_chain_id)
    end
    cfg.chain_id = rpc_chain_id

    machine.check_source(cfg.bootstrap_dir, cfg.state_source)
    local input_count = l1:input_count_at(cfg.bootstrap_block)
    if input_count == 0 and machine.root_hash(cfg.bootstrap_dir) ~= l1:template_hash() then
        errors.operator("no input precedes block %d, so the bootstrap machine must be the application's "
            .. "template, but its root hash differs from the on-chain template hash", cfg.bootstrap_block)
    end

    local head = store:checkpoint_dir(cfg.bootstrap_block, input_count)
    machine.clone(cfg.bootstrap_dir, work .. "/bootstrap")
    machine.publish(work .. "/bootstrap", head)
    store:write_json(config.persisted(cfg), "config.json")
    return {
        kind = "initialized",
        head = { block = cfg.bootstrap_block, input_count = input_count, dir = head },
    }
end

return bootstrap
