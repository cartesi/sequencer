-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local http = {}

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
