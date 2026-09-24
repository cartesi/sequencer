-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

--- Decoding of InputBox `InputAdded` logs and their `EvmAdvance` envelope.
--- The raw envelope is what the canonical machine receives; the decoded
--- fields let the L1 reader check it before feeding it.

local abi = {}

local WORD_HEX_LEN = 64
local MAX_EXACT_INTEGER = (1 << 53) - 1

local function strip_0x(value)
    assert(type(value) == "string", "hex value must be a string")
    return (value:gsub("^0[xX]", ""))
end

local function assert_hex(value)
    if value:match("^[0-9a-fA-F]*$") == nil then
        error("invalid hex string")
    end
end

local function word_at(hex, index)
    local start = (index * WORD_HEX_LEN) + 1
    local word = hex:sub(start, start + WORD_HEX_LEN - 1)
    if #word ~= WORD_HEX_LEN then
        error("ABI word out of bounds")
    end
    return word
end

--- A uint256 word that must fit an exact Lua integer (block numbers, input
--- indices, chain ids, offsets).
local function uint_word_to_integer(word)
    local value = 0
    for i = 1, #word do
        value = (value * 16) + tonumber(word:sub(i, i), 16)
        if value > MAX_EXACT_INTEGER then
            error("uint value too large for an exact integer")
        end
    end
    return value
end

local function address_from_word(word)
    if word:sub(1, 24) ~= string.rep("0", 24) then
        error("address word has non-zero high bytes")
    end
    return "0x" .. word:sub(25):lower()
end

function abi.bytes_from_hex(hex)
    hex = strip_0x(hex)
    assert_hex(hex)
    if (#hex % 2) ~= 0 then
        error("hex string must have even length")
    end
    return (hex:gsub("..", function(byte)
        return string.char(tonumber(byte, 16))
    end))
end

function abi.hex_from_bytes(bytes)
    return (bytes:gsub(".", function(char)
        return string.format("%02x", char:byte())
    end))
end

--- The integer a single returned uint256 word encodes (an `eth_call` result).
function abi.decode_uint(encoded)
    local hex = strip_0x(encoded)
    assert_hex(hex)
    return uint_word_to_integer(word_at(hex, 0))
end

local function dynamic_bytes_at(hex, offset_word_index)
    local offset = uint_word_to_integer(word_at(hex, offset_word_index))
    if (offset % 32) ~= 0 then
        error("dynamic bytes offset is not word-aligned")
    end
    local offset_words = offset // 32
    local len = uint_word_to_integer(word_at(hex, offset_words))
    local data_start = ((offset_words + 1) * WORD_HEX_LEN) + 1
    local data_hex = hex:sub(data_start, data_start + (len * 2) - 1)
    if #data_hex ~= len * 2 then
        error("dynamic bytes out of bounds")
    end
    return abi.bytes_from_hex(data_hex)
end

--- The bytes a returned dynamic `bytes` value encodes (an `eth_call` result).
function abi.decode_bytes(encoded)
    local hex = strip_0x(encoded)
    assert_hex(hex)
    return dynamic_bytes_at(hex, 0)
end

--- The address in a 32-byte ABI word.
function abi.decode_address_word(word_bytes)
    return address_from_word(abi.hex_from_bytes(word_bytes))
end

--- Decode `EvmAdvance(chainId, appContract, msgSender, blockNumber,
--- blockTimestamp, prevRandao, index, payload)` calldata.
function abi.decode_evm_advance(raw_input)
    local hex = abi.hex_from_bytes(raw_input)
    -- Calldata: skip the 4-byte selector.
    if (#hex % WORD_HEX_LEN) == 8 then
        hex = hex:sub(9)
    end
    return {
        chain_id = uint_word_to_integer(word_at(hex, 0)),
        app_contract = address_from_word(word_at(hex, 1)),
        msg_sender = address_from_word(word_at(hex, 2)),
        block_number = uint_word_to_integer(word_at(hex, 3)),
        index = uint_word_to_integer(word_at(hex, 6)),
        payload = dynamic_bytes_at(hex, 7),
    }
end

--- The raw input a log carries, plus its decoded envelope.
function abi.decode_input_added_log(log)
    if type(log) ~= "table" or type(log.data) ~= "string" then
        error("log.data is required")
    end
    local hex = strip_0x(log.data)
    assert_hex(hex)
    local raw_input = dynamic_bytes_at(hex, 0)
    local input = abi.decode_evm_advance(raw_input)
    input.raw = raw_input
    return input
end

return abi
