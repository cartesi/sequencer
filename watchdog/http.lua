-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- HTTP client via in-tree lua-curl / lcurl (libcurl on the host).

local http = {}

local function parse_response_headers(header_lines)
    local headers = {}
    for _, line in ipairs(header_lines) do
        local name, value = line:match("^([^:]+):%s*(.+)$")
        if name and value then
            headers[name:lower()] = value
        end
    end
    return headers
end

function http.format_request_error(method, url, err)
    return string.format("%s %s failed: %s", method, url, tostring(err))
end

local function request_method(opts)
    if opts.method then
        return opts.method
    end
    if opts.post then
        return "POST"
    end
    return "GET"
end

function http.new_curl()
    local ok, curl = pcall(require, "cURL")
    if not ok then
        ok, curl = pcall(require, "lcurl")
    end
    if not ok then
        error(
            "lua-curl binding not found; run: just watchdog-lua-deps "
                .. "(requires libcurl dev headers on the host)"
        )
    end

    local client = {}

    local function perform_request(opts)
        local chunks = {}
        local header_lines = {}
        local header_list = {}
        for key, value in pairs(opts.headers or {}) do
            table.insert(header_list, key .. ": " .. value)
        end

        local easy = curl.easy({
            url = opts.url,
            post = opts.post,
            postfields = opts.postfields,
            httpheader = header_list,
            timeout = 30,
            writefunction = function(chunk)
                table.insert(chunks, chunk)
                return #chunk
            end,
            headerfunction = function(line)
                local trimmed = line:gsub("\r?\n$", "")
                if trimmed ~= "" then
                    table.insert(header_lines, trimmed)
                end
                return #line
            end,
        })

        local ok_perform, err = pcall(function()
            easy:perform()
        end)
        if not ok_perform then
            easy:close()
            return nil, http.format_request_error(request_method(opts), opts.url, err)
        end

        local status = easy:getinfo_response_code()
        easy:close()
        return {
            status = status,
            body = table.concat(chunks),
            headers = parse_response_headers(header_lines),
        }
    end

    function client.post(_self, url, body, headers)
        return perform_request({
            url = url,
            post = true,
            postfields = body,
            headers = headers,
        })
    end

    function client.get(_self, url, headers)
        return perform_request({
            url = url,
            headers = headers,
        })
    end

    return client
end

function http.new()
    return http.new_curl()
end

return http
