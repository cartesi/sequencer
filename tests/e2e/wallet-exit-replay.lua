-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Replay the real InputBox stream to produce the exit scenario's checkpoint.

package.cpath = ".deps/lua/?.so;" .. package.cpath
local json = require("watchdog.json").new()
local cfg = {
    rpc_url = arg[1], app_address = arg[2]:lower(), input_box_address = arg[3]:lower(),
    chain_id = 31337, state_source = { kind = "inspect" },
}
local rpc = require("watchdog.jsonrpc").new(require("watchdog.http").new(), json, cfg.rpc_url)
local result = require("watchdog.replay").run(cfg, {
    machine = require("watchdog.machine").new(),
    l1 = require("watchdog.l1").new(rpc, cfg),
}, { from_dir = arg[4], from_block = 0, to_block = assert(tonumber(arg[5])), out_dir = arg[6] })
assert(not result.stop, "canonical guest stopped during exit replay")
io.write(json.encode(result), "\n")
