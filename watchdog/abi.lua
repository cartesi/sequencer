-- (c) Cartesi and individual authors (see AUTHORS)
-- SPDX-License-Identifier: Apache-2.0 (see LICENSE)

local abi = {}

local WORD_HEX_LEN = 64

local function strip_0x(value)
    assert(type(value) == "string", "hex value must be a string")
    if value:sub(1, 2) == "0x" or value:sub(1, 2) == "0X" then
        return value:sub(3)
    end
    return value
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

local function uint_word_to_number(word)
    local value = 0
    for i = 1, #word do
        local nibble = tonumber(word:sub(i, i), 16)
        value = (value * 16) + nibble
        if value > 9007199254740991 then
            error("uint value too large for precise Lua number")
        end
    end
    return value
end

local function uint_word_to_hex(word)
    local stripped = word:gsub("^0+", "")
    if stripped == "" then
        return "0x0"
    end
    return "0x" .. stripped:lower()
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

function abi.decode_single_dynamic_bytes(encoded)
    local hex = strip_0x(encoded)
    assert_hex(hex)
    local offset = uint_word_to_number(word_at(hex, 0))
    if (offset % 32) ~= 0 then
        error("dynamic bytes offset is not word-aligned")
    end

    local offset_words = offset // 32
    local len = uint_word_to_number(word_at(hex, offset_words))
    local data_hex_start = ((offset_words + 1) * WORD_HEX_LEN) + 1
    local data_hex = hex:sub(data_hex_start, data_hex_start + (len * 2) - 1)
    if #data_hex ~= len * 2 then
        error("dynamic bytes payload out of bounds")
    end
    return abi.bytes_from_hex(data_hex)
end

function abi.decode_evm_advance_call(encoded)
    local hex = strip_0x(encoded)
    assert_hex(hex)

    -- EvmAdvanceCall is calldata, so accept and skip the 4-byte selector.
    if (#hex % WORD_HEX_LEN) == 8 then
        hex = hex:sub(9)
    end

    local payload_offset = uint_word_to_number(word_at(hex, 7))
    if (payload_offset % 32) ~= 0 then
        error("payload offset is not word-aligned")
    end

    local payload_offset_words = payload_offset // 32
    local payload_len = uint_word_to_number(word_at(hex, payload_offset_words))
    local payload_hex_start = ((payload_offset_words + 1) * WORD_HEX_LEN) + 1
    local payload_hex = hex:sub(payload_hex_start, payload_hex_start + (payload_len * 2) - 1)
    if #payload_hex ~= payload_len * 2 then
        error("payload out of bounds")
    end

    return {
        chain_id_hex = uint_word_to_hex(word_at(hex, 0)),
        app_contract = address_from_word(word_at(hex, 1)),
        msg_sender = address_from_word(word_at(hex, 2)),
        block_number = uint_word_to_number(word_at(hex, 3)),
        block_timestamp_hex = uint_word_to_hex(word_at(hex, 4)),
        prev_randao_hex = uint_word_to_hex(word_at(hex, 5)),
        index_hex = uint_word_to_hex(word_at(hex, 6)),
        payload = abi.bytes_from_hex(payload_hex),
    }
end

function abi.decode_input_added_log(log)
    if type(log) ~= "table" or type(log.data) ~= "string" then
        error("log.data is required")
    end
    local input = abi.decode_single_dynamic_bytes(log.data)
    local decoded = abi.decode_evm_advance_call(abi.hex_from_bytes(input))
    decoded.raw_input = input
    return decoded
end

return abi
