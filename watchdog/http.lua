-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local http = {}

local function shell_quote(value)
    return "'" .. string.gsub(value, "'", "'\\''") .. "'"
end

--- HTTP client backed by the `curl` binary (for dev shells without lua-curl).
function http.new_cli()
    local client = {}

    local function run_curl(method, url, body, headers)
        local cmd = {
            "curl",
            "-sS",
            "-w",
            "%{http_code}",
            "-X",
            method,
        }
        if body then
            table.insert(cmd, "--data-binary")
            table.insert(cmd, shell_quote(body))
        end
        table.insert(cmd, shell_quote(url))
        for key, value in pairs(headers or {}) do
            table.insert(cmd, "-H")
            table.insert(cmd, shell_quote(key .. ": " .. value))
        end

        local shell_cmd = table.concat(cmd, " ") .. " 2>&1"
        local handle = io.popen(shell_cmd, "r")
        if not handle then
            return nil, "failed to start curl"
        end
        local output = handle:read("*a")
        local ok_exit, exit_type, exit_code = handle:close()
        if not output or output == "" then
            return nil, "empty curl response"
        end
        local status_line = output:match("(%d%d%d)$")
        if not status_line then
            return nil, string.format(
                "curl response missing status trailer (exit=%s/%s): %s",
                tostring(exit_type),
                tostring(exit_code),
                output
            )
        end
        local body_text = output:sub(1, #output - #status_line)
        local status = tonumber(status_line)
        if not ok_exit and status == 0 then
            return nil, string.format("curl failed: %s", output)
        end
        return {
            status = status,
            body = body_text,
            headers = {},
        }
    end

    function client.post(_self, url, body, headers)
        return run_curl("POST", url, body, headers)
    end

    function client.get(_self, url, headers)
        return run_curl("GET", url, nil, headers)
    end

    return client
end

function http.new_auto()
    local ok = pcall(function()
        require("cURL")
    end)
    if ok then
        return http.new_curl()
    end
    ok = pcall(function()
        require("lcurl")
    end)
    if ok then
        return http.new_curl()
    end
    return http.new_cli()
end

function http.new_curl()
    local ok, curl = pcall(require, "cURL")
    if not ok then
        ok, curl = pcall(require, "lcurl")
    end
    if not ok then
        error("lua-curl binding not found; install lua-curl/lcurl or inject an http adapter")
    end

    local client = {}

    function client.post(_self, url, body, headers)
        local chunks = {}
        local header_list = {}
        for key, value in pairs(headers or {}) do
            table.insert(header_list, key .. ": " .. value)
        end

        local easy = curl.easy({
            url = url,
            post = true,
            postfields = body,
            httpheader = header_list,
            timeout = 30,
            writefunction = function(chunk)
                table.insert(chunks, chunk)
                return #chunk
            end,
        })

        local ok_perform, err = pcall(function()
            easy:perform()
        end)
        if not ok_perform then
            easy:close()
            return nil, tostring(err)
        end

        local status = easy:getinfo_response_code()
        easy:close()
        return {
            status = status,
            body = table.concat(chunks),
            headers = {},
        }
    end

    function client.get(_self, url, headers)
        local chunks = {}
        local header_list = {}
        for key, value in pairs(headers or {}) do
            table.insert(header_list, key .. ": " .. value)
        end

        local easy = curl.easy({
            url = url,
            httpheader = header_list,
            timeout = 30,
            writefunction = function(chunk)
                table.insert(chunks, chunk)
                return #chunk
            end,
        })

        local ok_perform, err = pcall(function()
            easy:perform()
        end)
        if not ok_perform then
            easy:close()
            return nil, tostring(err)
        end

        local status = easy:getinfo_response_code()
        easy:close()
        return {
            status = status,
            body = table.concat(chunks),
            headers = {},
        }
    end

    return client
end

return http
