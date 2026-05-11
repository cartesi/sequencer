-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local machine = require("watchdog.machine")

local machine_cli = {}

local function shell_quote(value)
    value = tostring(value)
    return "'" .. value:gsub("'", "'\\''") .. "'"
end

local function mkdir_p(path)
    local ok = os.execute("mkdir -p " .. shell_quote(path))
    if ok ~= true and ok ~= 0 then
        return nil, "mkdir failed: " .. path
    end
    return true
end

local function write_all(path, data)
    local file, err = io.open(path, "wb")
    if not file then
        return nil, err
    end
    file:write(data)
    file:close()
    return true
end

local function run_command(command)
    local ok = os.execute(command)
    if ok == true or ok == 0 then
        return true
    end
    return nil, "command failed: " .. command
end

local function new_work_dir(base_dir)
    base_dir = base_dir or "/tmp"
    return string.format(
        "%s/watchdog-cm-%d-%d",
        base_dir:gsub("/+$", ""),
        os.time(),
        math.random(1000000)
    )
end

function machine_cli.new(opts)
    opts = opts or {}
    local executable = opts.executable or "cartesi-machine"
    local work_dir = opts.work_dir or "/tmp"

    local binding = {}

    function binding.load_snapshot(snapshot_dir)
        assert(type(snapshot_dir) == "string" and snapshot_dir ~= "", "snapshot_dir is required")
        local instance = {
            source_snapshot_dir = snapshot_dir,
            work_dir = new_work_dir(work_dir),
            input_dir = nil,
            input_count = 0,
        }
        local ok, err = mkdir_p(instance.work_dir)
        if not ok then
            return nil, err
        end
        instance.input_dir = instance.work_dir .. "/inputs"
        ok, err = mkdir_p(instance.input_dir)
        if not ok then
            return nil, err
        end
        return instance
    end

    function binding.feed_input(instance, input)
        assert(type(instance) == "table", "machine instance is required")
        assert(type(input) == "table", "input is required")
        local raw_input = input.raw_input
        if type(raw_input) ~= "string" then
            return nil, "input.raw_input is required"
        end

        local path = string.format("%s/input-%d.bin", instance.input_dir, instance.input_count)
        local ok, err = write_all(path, raw_input)
        if not ok then
            return nil, err
        end
        instance.input_count = instance.input_count + 1
        return true
    end

    function binding.inspect_state(_instance)
        -- Inspect requires rollback semantics around the loaded snapshot. The
        -- advance-only watchdog mode does not call this; compare mode remains
        -- deferred until the canonical app inspect contract exists.
        return nil, "CM inspect is not implemented for the CLI adapter yet"
    end

    function binding.save_snapshot(instance, snapshot_dir)
        assert(type(instance) == "table", "machine instance is required")
        assert(type(snapshot_dir) == "string" and snapshot_dir ~= "", "snapshot_dir is required")

        local command = table.concat({
            shell_quote(executable),
            "--no-rollback",
            "--load=" .. shell_quote(instance.source_snapshot_dir) .. ",sharing:none",
            "--cmio-advance-state=input:" .. shell_quote(instance.input_dir .. "/input-%i.bin")
                .. ",input_index_begin:0,input_index_end:" .. tostring(instance.input_count),
            "--store=" .. shell_quote(snapshot_dir),
            "--quiet",
        }, " ")

        return run_command(command)
    end

    return machine.new(binding)
end

machine_cli._private = {
    shell_quote = shell_quote,
}

return machine_cli
