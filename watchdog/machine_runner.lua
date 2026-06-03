-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Cartesi Machine driver: load, incremental advance, inspect, dump.

local machine_runner = {}

local function assert_contiguous_advance(instance, range)
    assert(type(range) == "table", "range is required")
    assert(type(range.from_block) == "number", "range.from_block is required")
    assert(type(range.to_block) == "number", "range.to_block is required")
    if range.from_block > range.to_block then
        error("advance range is empty or inverted")
    end
    local expected_from = (instance.reference_block or 0) + 1
    if range.from_block ~= expected_from then
        error(string.format(
            "non-contiguous advance: expected from_block %d, got %d",
            expected_from,
            range.from_block
        ))
    end
end

function machine_runner.new(binding)
    assert(type(binding) == "table", "Cartesi Machine binding is required")
    assert(type(binding.load_snapshot) == "function", "binding.load_snapshot is required")
    assert(type(binding.advance_inputs) == "function", "binding.advance_inputs is required")
    assert(type(binding.inspect_state) == "function", "binding.inspect_state is required")
    assert(type(binding.store_snapshot) == "function", "binding.store_snapshot is required")

    local driver = { binding = binding }

    function driver:load(path, reference_block)
        local instance, err = self.binding.load_snapshot(path)
        if not instance then
            return nil, err
        end
        instance.reference_block = reference_block or instance.reference_block or 0
        return instance
    end

    function driver:advance(instance, inputs, range)
        assert_contiguous_advance(instance, range)
        local ok, err = self.binding.advance_inputs(instance, inputs or {}, range)
        if not ok then
            return nil, err
        end
        instance.reference_block = range.to_block
        return true
    end

    function driver:inspect(instance)
        return self.binding.inspect_state(instance)
    end

    function driver:dump(instance, path, reference_block)
        assert(type(reference_block) == "number", "reference_block is required for dump")
        if instance.reference_block ~= reference_block then
            error(string.format(
                "dump reference_block %d does not match instance reference_block %d",
                reference_block,
                instance.reference_block or -1
            ))
        end
        return self.binding.store_snapshot(instance, path)
    end

    -- Legacy helpers used by machine_cli tests and older unit tests.
    function driver:feed_inputs(instance, inputs)
        local from = (instance.reference_block or 0) + 1
        return self:advance(instance, inputs, { from_block = from, to_block = from })
    end

    function driver:inspect_state(instance)
        return self:inspect(instance)
    end

    function driver:save(instance, path)
        assert(instance.reference_block ~= nil, "cannot dump before advance establishes reference_block")
        return self:dump(instance, path, instance.reference_block)
    end

    return driver
end

return machine_runner
