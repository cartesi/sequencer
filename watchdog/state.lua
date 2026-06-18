-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local state = {}

local function join(...)
    local parts = { ... }
    return table.concat(parts, "/"):gsub("//+", "/")
end

local function shell_quote(value)
    value = tostring(value)
    return "'" .. value:gsub("'", "'\\''") .. "'"
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

local function mkdir_p(path)
    local ok = os.execute("mkdir -p " .. shell_quote(path))
    if ok ~= true and ok ~= 0 then
        return nil, "mkdir failed: " .. path
    end
    return true
end

function state.ensure_dir(dir)
    assert(type(dir) == "string" and dir ~= "", "state dir is required")
    return mkdir_p(dir)
end

function state.write_json_atomic(dir, name, value, json)
    assert(type(dir) == "string" and dir ~= "", "state dir is required")
    assert(type(name) == "string" and name ~= "", "file name is required")
    assert(type(json) == "table" and type(json.encode) == "function", "json encoder is required")

    local ok, err = state.ensure_dir(dir)
    if not ok then
        return nil, err
    end

    local path = join(dir, name)
    local tmp = path .. ".tmp"
    ok, err = write_all(tmp, json.encode(value) .. "\n")
    if not ok then
        return nil, err
    end
    ok, err = os.rename(tmp, path)
    if not ok then
        return nil, err
    end
    return true
end

function state.read_json(dir, name, json)
    assert(type(dir) == "string" and dir ~= "", "state dir is required")
    assert(type(name) == "string" and name ~= "", "file name is required")
    assert(type(json) == "table" and type(json.decode) == "function", "json decoder is required")

    local data, err = read_all(join(dir, name))
    if not data then
        return nil, err
    end
    local decoded
    local ok = pcall(function()
        decoded = json.decode(data)
    end)
    if not ok or type(decoded) ~= "table" then
        return nil, "invalid " .. name
    end
    return decoded
end

return state
