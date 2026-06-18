-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local checkpoint = {}

checkpoint.HEAD_FILE = "head.json"

local function join(...)
    local parts = { ... }
    return table.concat(parts, "/"):gsub("//+", "/")
end

local function read_all(path)
    local file, err = io.open(path, "rb")
    if not file then
        return nil, err
    end
    local data = file:read("*a")
    file:close()
    return data
end

local function write_all(path, data)
    local file, err = io.open(path, "wb")
    if not file then
        return nil, err
    end
    local ok, write_err = file:write(data)
    if not ok then
        file:close()
        return nil, write_err
    end
    ok, err = file:close()
    if not ok then
        return nil, err
    end
    return true
end

local function path_exists(path)
    local ok, _, code = os.rename(path, path)
    return ok or code == 13
end

local function shell_quote(value)
    value = tostring(value)
    return "'" .. value:gsub("'", "'\\''") .. "'"
end

local function json_escape(value)
    return value:gsub("\\", "\\\\"):gsub('"', '\\"'):gsub("\n", "\\n")
end

local function manifest_json(manifest)
    local fields = {
        string.format('"safe_block":%d', manifest.safe_block),
        string.format('"created_at":"%s"', json_escape(manifest.created_at or os.date("!%Y-%m-%dT%H:%M:%SZ"))),
    }
    if manifest.cm_image_hash then
        table.insert(fields, string.format('"cm_image_hash":"%s"', json_escape(manifest.cm_image_hash)))
    end
    return "{" .. table.concat(fields, ",") .. "}\n"
end

local function pointer_json(relative_path)
    return string.format('{"checkpoint":"%s"}\n', json_escape(relative_path))
end

local function parse_pointer(data)
    return data:match('"checkpoint"%s*:%s*"([^"]+)"')
end

local function validate_pointer(relative_path)
    if type(relative_path) ~= "string" or not relative_path:match("^checkpoints/%d+$") then
        return nil, "invalid checkpoint pointer"
    end
    return relative_path
end

-- Relative path head.json points at (e.g. "checkpoints/000...019"), or nil
-- if there is no pointer yet. Used by write() to find the checkpoint it
-- supersedes without enumerating the directory.
local function read_pointer(dir)
    local data = read_all(join(dir, checkpoint.HEAD_FILE))
    if not data then
        return nil
    end
    return validate_pointer(parse_pointer(data))
end

local function relative_checkpoint_path(safe_block)
    return join("checkpoints", string.format("%020d", safe_block))
end

function checkpoint.safe_block_from_manifest(manifest_data)
    local value = tostring(manifest_data or ""):match('"safe_block"%s*:%s*(%d+)')
    if not value then
        return nil, "manifest missing safe_block"
    end
    return tonumber(value)
end

function checkpoint.load(dir)
    local pointer_path = join(dir, checkpoint.HEAD_FILE)
    if not path_exists(pointer_path) then
        return nil, "missing " .. checkpoint.HEAD_FILE
    end

    local pointer_data, err = read_all(pointer_path)
    if not pointer_data then
        return nil, err
    end
    local relative_path = parse_pointer(pointer_data)
    local pointer_err
    relative_path, pointer_err = validate_pointer(relative_path)
    if not relative_path then
        return nil, pointer_err
    end
    local checkpoint_dir = join(dir, relative_path)
    local manifest, manifest_err = read_all(join(checkpoint_dir, "manifest.json"))
    if not manifest then
        return nil, manifest_err
    end
    local safe_block, safe_block_err = checkpoint.safe_block_from_manifest(manifest)
    if not safe_block then
        return nil, safe_block_err
    end
    return {
        path = checkpoint_dir,
        manifest_json = manifest,
        safe_block = safe_block,
        snapshot_dir = join(checkpoint_dir, "snapshot"),
    }
end

function checkpoint.prepare(dir, safe_block)
    assert(type(safe_block) == "number", "safe_block must be a number")

    local relative_path = relative_checkpoint_path(safe_block)
    local full_path = join(dir, relative_path)
    local snapshot_dir = join(full_path, "snapshot")

    -- Crash mid-store can leave a half-written snapshot; clear before retrying this block.
    local rm_ok = os.execute("rm -rf " .. shell_quote(snapshot_dir))
    if rm_ok ~= true and rm_ok ~= 0 then
        return nil, "rm failed: " .. snapshot_dir
    end

    local ok = os.execute("mkdir -p " .. shell_quote(full_path))
    if ok ~= true and ok ~= 0 then
        return nil, "mkdir failed: " .. full_path
    end

    return {
        path = full_path,
        snapshot_dir = snapshot_dir,
        relative_path = relative_path,
    }
end

function checkpoint.commit_prepared(dir, prepared, safe_block, manifest)
    assert(type(prepared) == "table", "prepared checkpoint is required")
    assert(type(safe_block) == "number", "safe_block must be a number")
    manifest = manifest or {}
    manifest.safe_block = safe_block

    local ok, err = write_all(join(prepared.path, "manifest.json"), manifest_json(manifest))
    if not ok then
        return nil, err
    end

    local tmp_pointer = join(dir, checkpoint.HEAD_FILE .. ".tmp")
    ok, err = write_all(tmp_pointer, pointer_json(prepared.relative_path))
    if not ok then
        return nil, err
    end
    ok, err = os.rename(tmp_pointer, join(dir, checkpoint.HEAD_FILE))
    if not ok then
        return nil, err
    end

    return {
        path = prepared.path,
        snapshot_dir = prepared.snapshot_dir,
    }
end

function checkpoint.write(dir, safe_block, snapshot_writer, manifest)
    assert(type(snapshot_writer) == "function", "snapshot_writer must be a function")
    local relative_path = relative_checkpoint_path(safe_block)
    local superseded = read_pointer(dir)
    if superseded == relative_path then
        return nil, "refusing to rewrite selected checkpoint: " .. relative_path
    end

    local prepared, prepare_err = checkpoint.prepare(dir, safe_block)
    if not prepared then
        return nil, prepare_err
    end
    local ok, err = snapshot_writer(prepared.snapshot_dir)
    if not ok then
        return nil, err
    end

    -- The checkpoint head.json points at before the flip is the one we are
    -- superseding (nil on the first write).

    local committed, commit_err = checkpoint.commit_prepared(dir, prepared, safe_block, manifest)
    if not committed then
        return nil, commit_err
    end

    -- Reclaim the superseded checkpoint AFTER the atomic pointer flip, so
    -- head.json always names a complete checkpoint (a crash here leaves at
    -- most one stale dir, never an orphaned pointer). Best-effort: GC must not
    -- fail a cycle whose compare and checkpoint write already succeeded.
    if superseded and superseded ~= prepared.relative_path then
        os.execute("rm -rf " .. shell_quote(join(dir, superseded)))
    end

    return committed
end

return checkpoint
