-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local retry = {}

local function default_sleep(seconds)
    seconds = tonumber(seconds)
    if not seconds or seconds <= 0 then
        return
    end
    local ok, how, code = os.execute("exec sleep " .. tostring(seconds))
    if not ok and how == "signal" and code == 2 then
        os.exit(130, true)
    end
end

function retry.with_retries(fn, opts)
    opts = opts or {}
    local attempts = math.max(1, opts.attempts or 1)
    local delay_sec = opts.delay_sec or 0
    local should_retry = opts.should_retry or function(_err)
        return true
    end
    local sleep = opts.sleep or default_sleep

    local last_err
    for attempt = 1, attempts do
        local result, err = fn(attempt)
        if result then
            return result
        end
        last_err = err
        if not should_retry(err) then
            break
        end
        if attempt < attempts then
            sleep(delay_sec)
        end
    end
    return nil, last_err
end

return retry
