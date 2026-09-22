-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- The watchdog state directory:
---
---     config.json                  deployment identity and state source (init)
---     checkpoints/<block>-<count>/ stored machines the sequencer agreed with;
---                                  the newest is the head
---     work/                        scratch for the running command
---     incident/                    the latched divergence, if any
---     incidents/<id>/              cleared incidents
---     last_tick.json, status.prom  the last tick's outcome
---
--- A checkpoint's name carries its L1 block and the number of InputBox inputs
--- through that block, and the machine layer publishes it with an atomic,
--- fsynced rename: the head needs no pointer file. JSON files are written
--- atomically but without fsync; each is either rebuilt by the next command
--- (a lost latch re-latches, a lost tick record is rewritten) or written once
--- by init, which is re-run after a crash.

local lfs = require("lfs")
local errors = require("watchdog.errors")

local store = {}

--- `<block>-<input count>`, each zero-padded to 20 digits.
local function parse_checkpoint_name(name)
    local block, count = name:match("^(%d+)%-(%d+)$")
    if block and #block == 20 and #count == 20 then
        return math.tointeger(tonumber(block)), math.tointeger(tonumber(count))
    end
end

local function is_dir(path)
    return lfs.attributes(path, "mode") == "directory"
end

local function exists(path)
    return lfs.symlinkattributes(path, "mode") ~= nil
end
store.exists = exists

local function mkdir(path)
    if not is_dir(path) then
        local ok, err = lfs.mkdir(path)
        if not ok and not is_dir(path) then
            errors.operator("cannot create %s: %s", path, tostring(err))
        end
    end
end

--- Remove a file or directory tree (leftovers of an interrupted command).
local function remove_tree(path)
    local mode = lfs.symlinkattributes(path, "mode")
    if mode == nil then
        return
    end
    if mode == "directory" then
        for entry in lfs.dir(path) do
            if entry ~= "." and entry ~= ".." then
                remove_tree(path .. "/" .. entry)
            end
        end
        assert(lfs.rmdir(path))
    else
        assert(os.remove(path))
    end
end
store.remove_tree = remove_tree

local function read_file(path)
    local file = io.open(path, "rb")
    if not file then
        return nil
    end
    local data = file:read("a")
    file:close()
    return data
end

function store.write_file_atomic(path, data)
    local tmp = path .. ".tmp"
    local file = assert(io.open(tmp, "wb"))
    assert(file:write(data))
    assert(file:close())
    assert(os.rename(tmp, path))
end

function store.open(dir, json)
    assert(is_dir(dir), "state directory must exist: " .. dir)
    local s = { dir = dir }

    function s:path(...)
        return table.concat({ dir, ... }, "/")
    end

    function s:exists(...)
        return exists(self:path(...))
    end

    function s:read_json(...)
        local path = self:path(...)
        local data = read_file(path)
        if data == nil then
            return nil
        end
        local ok, decoded = pcall(json.decode, data)
        if not ok or type(decoded) ~= "table" then
            errors.operator("%s is not valid JSON", path)
        end
        return decoded
    end

    function s:write_json(value, ...)
        store.write_file_atomic(self:path(...), json.encode(value) .. "\n")
    end

    function s:write_text(text, ...)
        store.write_file_atomic(self:path(...), text)
    end

    function s:mkdir(...)
        mkdir(self:path(...))
        return self:path(...)
    end

    function s:remove(...)
        remove_tree(self:path(...))
    end

    --- A fresh, empty scratch directory.
    function s:reset_work()
        remove_tree(self:path("work"))
        return self:mkdir("work")
    end

    --- Published checkpoints, oldest first.
    function s:checkpoints()
        local found = {}
        if is_dir(self:path("checkpoints")) then
            for entry in lfs.dir(self:path("checkpoints")) do
                local block, count = parse_checkpoint_name(entry)
                if block then
                    found[#found + 1] = { block = block, input_count = count, dir = self:path("checkpoints", entry) }
                elseif entry ~= "." and entry ~= ".." then
                    errors.operator("unexpected entry %s in %s", entry, self:path("checkpoints"))
                end
            end
        end
        table.sort(found, function(a, b)
            return a.block < b.block
        end)
        return found
    end

    function s:head()
        local all = self:checkpoints()
        return all[#all]
    end

    function s:checkpoint_dir(block, input_count)
        mkdir(self:path("checkpoints"))
        return self:path("checkpoints", string.format("%020d-%020d", block, input_count))
    end

    return s
end

return store
