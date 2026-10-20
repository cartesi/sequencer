-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Export withdrawal arguments from a trusted finalized canonical machine.
--- Run at repo root: lua5.4 scripts/wallet-exit-proof.lua SNAPSHOT OWNER

local cartesi = require("cartesi")
local abi = require("watchdog.abi")
local json = require("watchdog.json").new()
local start, log2_size, account_size = 0x90000000000000, 26, 64
local dir, owner = assert(arg[1], "snapshot required"), assert(arg[2], "owner required")
assert(owner:match("^0x%x+$") and #owner == 42, "owner must be a 20-byte address")
local wanted = abi.bytes_from_hex(owner)
assert(cartesi.VERSION == "0.21.0", "use the repository-pinned emulator version")
local m = cartesi.new()
m:load(dir, {}, cartesi.SHARING_NONE)
assert(m:verify_hash_tree(), "snapshot fails hash-tree verification")
local matching_range = false
for _, range in ipairs(m:get_initial_config().flash_drive or {}) do
    if range.label == "accounts" and range.start == start and range.length == (1 << log2_size) then
        matching_range = true
    end
end
assert(matching_range, "snapshot has no reference-wallet accounts drive")
local function siblings(proof, count)
    local result = {}
    for i = 1, count do
        result[#result + 1] = "0x" .. abi.hex_from_bytes(proof.sibling_hashes[i])
    end
    return result
end
local found
for index = 0, (1 << log2_size) // account_size - 1 do
    local account = m:read_memory(start + index * account_size, account_size)
    if account == string.rep("\0", account_size) then break end
    if account:sub(13, 32) == wanted then
        local drive = m:get_proof(start, log2_size)
        local proof = m:get_proof(start + index * account_size, 6)
        found = {
            machine_root = "0x" .. abi.hex_from_bytes(drive.root_hash),
            accounts_drive_root = "0x" .. abi.hex_from_bytes(drive.target_hash),
            accounts_drive_siblings = siblings(drive, #drive.sibling_hashes),
            account = "0x" .. abi.hex_from_bytes(account),
            account_index = index,
            account_root_siblings = siblings(proof, 20),
        }
        break
    end
end
m:destroy()
assert(found, "owner has no exit entitlement in this snapshot")
io.write(json.encode(found), "\n")
