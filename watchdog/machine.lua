-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local machine = {}

function machine.new(binding)
    assert(type(binding) == "table", "Cartesi Machine binding is required")
    assert(type(binding.load_snapshot) == "function", "binding.load_snapshot is required")
    assert(type(binding.feed_input) == "function", "binding.feed_input is required")
    assert(type(binding.inspect_state) == "function", "binding.inspect_state is required")
    assert(type(binding.save_snapshot) == "function", "binding.save_snapshot is required")

    local driver = { binding = binding }

    function driver:load(path)
        return self.binding.load_snapshot(path)
    end

    function driver:feed_inputs(instance, inputs)
        for _, input in ipairs(inputs) do
            -- Do not classify payload tags here. The scheduler inside the CM owns
            -- direct-input vs batch-submission semantics.
            local ok, err = self.binding.feed_input(instance, input)
            if not ok then
                return nil, err
            end
        end
        return true
    end

    function driver:inspect_state(instance)
        return self.binding.inspect_state(instance)
    end

    function driver:save(instance, path)
        assert(type(self.binding.save_snapshot) == "function", "binding.save_snapshot is required")
        return self.binding.save_snapshot(instance, path)
    end

    return driver
end

return machine
