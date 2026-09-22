-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- The divergence latch. A divergence writes its local evidence into
--- `incident/` and then `incident/divergence.json`: the marker is the latch.
--- The sequencer's comparison file is fetched afterwards, as best effort, so
--- a slow download never delays the latch. While the marker exists every tick
--- exits 2 without work, until an operator clears it with `clear`, which
--- archives the incident instead of deleting anything. Clearing is always
--- safe: if the cause persists, the next tick latches again. The incident
--- runbook (docs/watchdog/incident-runbook.md) owns the procedure.

local lfs = require("lfs")
local errors = require("watchdog.errors")

local incident = {}

local MARKER = "divergence.json"
local CHUNK = 1 << 20
local PAGE = 4096

--- The latched incident's marker, or nil. A marker that exists but cannot be
--- read (torn by a crash; JSON files are not fsynced) still latches.
function incident.marker(store)
    if not store:exists("incident", MARKER) then
        return nil
    end
    local ok, marker = pcall(store.read_json, store, "incident", MARKER)
    if not ok or type(marker) ~= "table" then
        return { kind = "unreadable_marker" }
    end
    return marker
end

--- Discard an interrupted latch (`incident/` without a marker) so the next
--- divergence can write its own evidence.
function incident.discard_interrupted(store)
    if store:exists("incident") and incident.marker(store) == nil then
        store:remove("incident")
    end
end

--- Offset of the first differing byte and the number of differing 4 KiB
--- pages of two files, streamed; a length difference counts from the shorter
--- end. `{ identical = true }` when the bytes agree: then the digests were
--- wrong, not the state.
function incident.compare_files(path_a, path_b)
    local a = assert(io.open(path_a, "rb"))
    local b = assert(io.open(path_b, "rb"))
    local first, pages, offset = nil, 0, 0
    while true do
        local ca, cb = a:read(CHUNK) or "", b:read(CHUNK) or ""
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
    a:close()
    b:close()
    if first then
        return { first_difference = first, differing_pages = pages }
    end
    return { identical = true }
end

--- Latch a divergence. `event` becomes the marker (plus `detected_at` and
--- evidence fields). `evidence` may carry:
---   machine            a stored machine directory to keep as incident/canonical
---   publish            function(from, to) that durably moves a stored machine
---   canonical_bytes    the canonical comparison bytes
---   sequencer          client whose comparison file is fetched after the latch,
---                      kept only if it still describes `event.target_block`
function incident.latch(store, event, evidence, now)
    store:remove("incident")
    store:mkdir("incident")
    event.detected_at = now
    local recorded = {}
    event.evidence = recorded

    if evidence.machine then
        evidence.publish(evidence.machine, store:path("incident", "canonical"))
        recorded.canonical_machine = "incident/canonical"
    end
    if evidence.canonical_bytes then
        local file = assert(io.open(store:path("incident", "canonical.bin"), "wb"))
        assert(file:write(evidence.canonical_bytes))
        assert(file:close())
        recorded.canonical_bytes = "incident/canonical.bin"
    end
    store:write_json(event, "incident", MARKER)

    if evidence.sequencer then
        local path = store:path("incident", "sequencer.bin")
        local downloaded, block = pcall(evidence.sequencer.download_state, evidence.sequencer, path .. ".tmp")
        if downloaded and block == event.target_block then
            assert(os.rename(path .. ".tmp", path))
            recorded.sequencer_bytes = "incident/sequencer.bin"
            if recorded.canonical_bytes then
                recorded.comparison = incident.compare_files(store:path("incident", "canonical.bin"), path)
            end
        else
            os.remove(path .. ".tmp")
            if downloaded then
                recorded.sequencer_bytes_missing = string.format("the sequencer moved on to block %d", block)
            else
                recorded.sequencer_bytes_missing = select(2, errors.classify(block))
            end
        end
        store:write_json(event, "incident", MARKER)
    end
    return event
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
