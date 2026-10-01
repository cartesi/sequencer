-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- HTTP client over the vendored lua-curl (`lcurl`). Requests return a
--- response table, or `nil, message` when no response arrived.
---
--- Small requests share a total timeout. Downloads stream to a file and are
--- bounded by a stall limit instead, since their size is the application's.

local http = {}

local REQUEST_TIMEOUT_SEC = 30
local CONNECT_TIMEOUT_SEC = 10
local STALL_BYTES_PER_SEC = 1024
local STALL_SEC = 60

local function parse_headers(lines)
    local headers = {}
    for _, line in ipairs(lines) do
        local name, value = line:match("^([^:]+):%s*(.-)%s*$")
        if name then
            headers[name:lower()] = value
        end
    end
    return headers
end

function http.new()
    local ok, curl = pcall(require, "lcurl")
    if not ok then
        error("lua-curl module `lcurl` not found; run: just watchdog-lua-deps")
    end

    local client = {}

    -- `opts`: url, method, body, headers, timeout (total seconds, nil for a
    -- stall-bounded download), write (receives each body chunk).
    local function perform(opts)
        local header_lines, header_list = {}, {}
        for key, value in pairs(opts.headers or {}) do
            header_list[#header_list + 1] = key .. ": " .. value
        end
        local easy = curl.easy({
            url = opts.url,
            httpheader = header_list,
            connecttimeout = CONNECT_TIMEOUT_SEC,
            writefunction = function(chunk)
                opts.write(chunk)
                return #chunk
            end,
            headerfunction = function(line)
                local trimmed = line:gsub("\r?\n$", "")
                if trimmed ~= "" then
                    header_lines[#header_lines + 1] = trimmed
                end
                return #line
            end,
        })
        if opts.body then
            easy:setopt_post(true)
            easy:setopt_postfields(opts.body)
        end
        if opts.timeout then
            easy:setopt_timeout(opts.timeout)
        else
            easy:setopt_low_speed_limit(STALL_BYTES_PER_SEC)
            easy:setopt_low_speed_time(STALL_SEC)
        end
        local performed, err = pcall(easy.perform, easy)
        local status = performed and easy:getinfo_response_code() or nil
        easy:close()
        if not performed then
            return nil, string.format("%s %s failed: %s", opts.body and "POST" or "GET", opts.url, tostring(err))
        end
        return { status = status, headers = parse_headers(header_lines) }
    end

    local function buffered(opts)
        local chunks = {}
        opts.write = function(chunk)
            chunks[#chunks + 1] = chunk
        end
        opts.timeout = opts.timeout or REQUEST_TIMEOUT_SEC
        local response, err = perform(opts)
        if response then
            response.body = table.concat(chunks)
        end
        return response, err
    end

    function client:get(url, opts)
        opts = opts or {}
        return buffered({ url = url, headers = opts.headers, timeout = opts.timeout })
    end

    function client:post(url, body, headers)
        return buffered({ url = url, body = body, headers = headers })
    end

    --- Stream the body into `path`. For a non-2xx status the response also
    --- carries the body's first bytes, for the error message.
    function client:download(url, path)
        local file = assert(io.open(path, "wb"))
        local response, err = perform({
            url = url,
            write = function(chunk)
                assert(file:write(chunk))
            end,
        })
        assert(file:close())
        if response and (response.status < 200 or response.status >= 300) then
            local body = assert(io.open(path, "rb"))
            response.body = body:read(512) or ""
            body:close()
        end
        return response, err
    end

    return client
end

return http
