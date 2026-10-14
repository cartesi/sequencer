-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- `init`: store a trusted canonical machine as the first checkpoint and persist
--- the configuration (docs/watchdog/README.md#commands).

local canonical = require("watchdog.canonical")
local config = require("watchdog.config")
local errors = require("watchdog.errors")

local bootstrap = {}

--- `deps`: store, machine, l1.
function bootstrap.run(cfg, deps)
    local store, machine, l1 = deps.store, deps.machine, deps.l1

    local existing = store:read_json("config.json")
    if existing then
        -- The same diagnosis `tick` would give a hand-edited config.
        config.load(store.dir, existing, {})
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

    -- No config.json. An interrupted init leaves at most its own checkpoint,
    -- at the bootstrap block; an agreed head is always later, so one here
    -- means config.json was lost, and deleting the head would discard agreement.
    for _, checkpoint in ipairs(store:checkpoints()) do
        if checkpoint.block ~= cfg.bootstrap_block then
            errors.operator("%s has checkpoint %s but no config.json; restore config.json or wipe the directory",
                store.dir, checkpoint.dir)
        end
    end
    local checkpoints = store:path("checkpoints")
    if cfg.bootstrap_dir:sub(1, #checkpoints + 1) == checkpoints .. "/" then
        errors.operator("CARTESI_WATCHDOG_CM_SNAPSHOT_DIR %s is inside %s, which init replaces",
            cfg.bootstrap_dir, checkpoints)
    end
    local work = store:reset_work()

    local rpc_chain_id = l1:chain_id()
    if cfg.chain_id and cfg.chain_id ~= rpc_chain_id then
        errors.operator("CARTESI_WATCHDOG_BLOCKCHAIN_ID is %d but the L1 RPC serves chain %d",
            cfg.chain_id, rpc_chain_id)
    end
    cfg.chain_id = rpc_chain_id
    cfg.input_box_address = l1:input_box_address()

    machine.check_source(cfg.bootstrap_dir, cfg.state_source)
    local input_count = canonical.trusted_start(machine, l1, cfg.bootstrap_dir, cfg.bootstrap_block).input_count

    machine.clone(cfg.bootstrap_dir, work .. "/bootstrap")
    store:remove("checkpoints")
    local head = store:checkpoint_dir(cfg.bootstrap_block, input_count)
    machine.publish(work .. "/bootstrap", head)
    store:write_json(config.persisted(cfg), "config.json")
    return {
        kind = "initialized",
        head = { block = cfg.bootstrap_block, input_count = input_count, dir = head },
    }
end

return bootstrap
