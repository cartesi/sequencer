-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- The divergence latch. Latching creates `incident/` and writes the latch
--- record `incident/divergence.json`, and nothing else: the directory is the
--- latch, so a record lost to a crash or a full disk still latches (as
--- `unreadable_marker`). The tick then signals the divergence, and only then
--- collects evidence into `incident/`, each item on its own, with progress in
--- `incident/evidence.json`: a slow or failed collection can neither delay
--- nor undo the latch. While latched every tick exits 2 without work, until
--- an operator clears it with `clear`, which archives the incident instead of
--- deleting anything. Clearing is always safe: if the cause persists, the next
--- tick latches again. The incident runbook (docs/watchdog/incident-runbook.md)
--- owns the procedure.

local lfs = require("lfs")
local errors = require("watchdog.errors")

local incident = {}

local MARKER = "divergence.json"
local EVIDENCE = "evidence.json"
local CHUNK = 1 << 20
local PAGE = 4096

--- The latched incident's record, or nil. An `incident/` whose record is
--- missing or unreadable (JSON files are not fsynced) still latches.
function incident.marker(store)
    if not store:exists("incident") then
        return nil
    end
    local ok, marker = pcall(store.read_json, store, "incident", MARKER)
    if not ok or type(marker) ~= "table" then
        return { kind = "unreadable_marker" }
    end
    return marker
end

--- The latched incident's evidence index, or nil when it has none.
function incident.evidence(store)
    local ok, index = pcall(store.read_json, store, "incident", EVIDENCE)
    if not ok then
        return { collection = "unreadable" }
    end
    return index
end

--- Offset of the first byte where `bytes` and the file at `path` differ, and
--- the number of differing 4 KiB pages, streaming the file; a length
--- difference counts from the shorter end. `{ identical = true }` when they
--- agree: then the digests were wrong, not the state.
function incident.compare(bytes, path)
    local file = assert(io.open(path, "rb"))
    local first, pages, offset = nil, 0, 0
    while true do
        local cb, err = file:read(CHUNK)
        if not cb and err then
            file:close()
            error(string.format("cannot read %s: %s", path, err), 0)
        end
        local ca = bytes:sub(offset + 1, offset + CHUNK)
        cb = cb or ""
        if ca == "" and cb == "" then
            break
        end
        if ca ~= cb then
            local n = math.max(#ca, #cb)
            for page = 0, (n - 1) // PAGE do
                local lo, hi = page * PAGE + 1, (page + 1) * PAGE
                local pa, pb = ca:sub(lo, hi), cb:sub(lo, hi)
                if pa ~= pb then
                    pages = pages + 1
                    if not first then
                        local i = 1
                        while i <= #pa and pa:byte(i) == pb:byte(i) do
                            i = i + 1
                        end
                        first = offset + lo - 1 + i - 1
                    end
                end
            end
        end
        offset = offset + math.max(#ca, #cb)
    end
    file:close()
    if first then
        return { first_difference = first, differing_pages = pages }
    end
    return { identical = true }
end

--- Latch a divergence: create `incident/`, the latch, and write `event`, plus
--- `detected_at`, as its record. Raises when `incident/` cannot be created;
--- returns the reason when only the record could not be written.
function incident.latch(store, event, now)
    event.detected_at = now
    store:mkdir("incident")
    local written, err = pcall(store.write_json, store, event, "incident", MARKER)
    if not written then
        return select(2, errors.classify(err))
    end
end

--- Collect the latched `event`'s evidence, the one item nothing can re-derive
--- first. `evidence` may carry:
---   sequencer        client whose comparison file is kept if it still
---                    describes `event.target_block`
---   canonical_bytes  the canonical comparison bytes: compared with the
---                    sequencer's file, then kept as canonical.bin
---   machine          a stored machine directory, kept as incident/canonical
---   publish          function(from, to) that durably moves a stored machine
--- `incident/evidence.json` records each item as it ends: its path or result,
--- or `<item>_missing` with the reason. Its `collection` is `running`, then
--- `finished`; a latched tick marks a collection it finds `running` as
--- `interrupted`. Final file names are complete by construction (renames);
--- `*.tmp` files are partial. Never raises; returns the index.
function incident.collect(store, event, evidence)
    local index = { collection = "running" }
    local function save()
        pcall(store.write_json, store, index, "incident", EVIDENCE)
    end
    local function keep(item, fn)
        local ok, value = pcall(fn)
        if ok then
            index[item] = value
        else
            index[item .. "_missing"] = string.format("%s: %s", errors.classify(value))
        end
        save()
    end
    save()
    if evidence.sequencer then
        keep("sequencer_bytes", function()
            local path = store:path("incident", "sequencer.bin")
            local ok, block = pcall(evidence.sequencer.download_state, evidence.sequencer, path .. ".tmp")
            if not ok or block ~= event.target_block then
                os.remove(path .. ".tmp")
                if not ok then
                    error(block, 0)
                end
                errors.transient("the sequencer moved on to block %d", block)
            end
            local renamed, err = os.rename(path .. ".tmp", path)
            if not renamed then
                os.remove(path .. ".tmp")
                error(err, 0)
            end
            return "incident/sequencer.bin"
        end)
        if index.sequencer_bytes and evidence.canonical_bytes then
            keep("comparison", function()
                return incident.compare(evidence.canonical_bytes, store:path("incident", "sequencer.bin"))
            end)
        end
    end
    if evidence.machine then
        keep("canonical_machine", function()
            evidence.publish(evidence.machine, store:path("incident", "canonical"))
            return "incident/canonical"
        end)
    end
    if evidence.canonical_bytes then
        keep("canonical_bytes", function()
            store:write_text(evidence.canonical_bytes, "incident", "canonical.bin")
            return "incident/canonical.bin"
        end)
    end
    index.collection = "finished"
    save()
    return index
end

--- Mark a collection still `running` as `interrupted`; true if it was. Only a
--- latched tick calls this, and the wrapper's lock lets it run only after the
--- latching tick ended.
function incident.mark_interrupted(store)
    local index = incident.evidence(store)
    if not (index and index.collection == "running") then
        return false
    end
    index.collection = "interrupted"
    store:write_json(index, "incident", EVIDENCE)
    return true
end

--- Archive the latched incident for `block` with the operator's reason.
function incident.clear(store, block, reason, now)
    local marker = incident.marker(store)
    if marker == nil then
        errors.operator("no divergence is latched")
    end
    -- An unreadable marker has no block to check; the operator names one.
    if marker.kind ~= "unreadable_marker" and marker.target_block ~= block then
        errors.operator("the latched divergence is for block %s, not %s; nothing cleared",
            tostring(marker.target_block), tostring(block))
    end
    store:write_json({ cleared_at = now, reason = reason }, "incident", "resolution.json")
    store:mkdir("incidents")
    local stamp = (marker.detected_at or now):gsub("[^%w]", "")
    local id = string.format("%s-%s-%d", stamp, marker.kind, block)
    local archive = store:path("incidents", id)
    if lfs.attributes(archive) then
        errors.operator("incident archive %s already exists", archive)
    end
    assert(os.rename(store:path("incident"), archive))
    return archive
end

return incident
