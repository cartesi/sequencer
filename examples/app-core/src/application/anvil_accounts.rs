// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::sync::OnceLock;

use alloy_primitives::{Address, hex};

const ANVIL_ACCOUNTS_RAW: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/anvil_accounts.txt"));

#[derive(Debug)]
struct ParsedAnvilAccounts {
    addresses: Vec<Address>,
    private_keys: Vec<String>,
}

static PARSED_ANVIL_ACCOUNTS: OnceLock<ParsedAnvilAccounts> = OnceLock::new();

pub fn prefunded_addresses() -> &'static [Address] {
    parsed_anvil_accounts().addresses.as_slice()
}

pub fn default_private_keys() -> &'static [String] {
    parsed_anvil_accounts().private_keys.as_slice()
}

fn parsed_anvil_accounts() -> &'static ParsedAnvilAccounts {
    PARSED_ANVIL_ACCOUNTS.get_or_init(|| {
        parse_accounts(ANVIL_ACCOUNTS_RAW)
            .unwrap_or_else(|reason| panic!("invalid anvil_accounts.txt: {reason}"))
    })
}

fn parse_accounts(raw: &str) -> Result<ParsedAnvilAccounts, String> {
    let mut addresses = Vec::new();
    let mut private_keys = Vec::new();

    for (line_no, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let address = parts
            .next()
            .ok_or_else(|| format!("line {}: missing address", line_no + 1))?;
        let private_key = parts
            .next()
            .ok_or_else(|| format!("line {}: missing private key", line_no + 1))?;
        if parts.next().is_some() {
            return Err(format!(
                "line {}: expected exactly two tokens: <address> <private_key>",
                line_no + 1
            ));
        }

        addresses.push(parse_hex_address(address, line_no + 1)?);
        validate_hex_token(private_key, 66, "private key", line_no + 1)?;
        private_keys.push(private_key.to_string());
    }

    if addresses.is_empty() {
        return Err("file does not contain any account records".to_string());
    }
    if addresses.len() != private_keys.len() {
        return Err("internal parser mismatch between addresses and keys".to_string());
    }

    Ok(ParsedAnvilAccounts {
        addresses,
        private_keys,
    })
}

fn parse_hex_address(value: &str, line_no: usize) -> Result<Address, String> {
    validate_hex_token(value, 42, "address", line_no)?;
    let decoded = hex::decode(value)
        .map_err(|err| format!("line {}: invalid address hex: {err}", line_no))?;
    if decoded.len() != 20 {
        return Err(format!(
            "line {}: invalid address byte length: expected 20, got {}",
            line_no,
            decoded.len()
        ));
    }
    Ok(Address::from_slice(decoded.as_slice()))
}

fn validate_hex_token(
    token: &str,
    expected_len: usize,
    field_name: &str,
    line_no: usize,
) -> Result<(), String> {
    if token.len() != expected_len {
        return Err(format!(
            "line {}: invalid {} length: expected {}, got {}",
            line_no,
            field_name,
            expected_len,
            token.len()
        ));
    }
    if !token.starts_with("0x") {
        return Err(format!(
            "line {}: {} must start with 0x",
            line_no, field_name
        ));
    }
    if !token.as_bytes().iter().skip(2).all(u8::is_ascii_hexdigit) {
        return Err(format!("line {}: {} must be hex", line_no, field_name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_accounts;

    #[test]
    fn parse_accounts_accepts_valid_lines() {
        let raw = "\
0x1111111111111111111111111111111111111111 0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
0x2222222222222222222222222222222222222222 0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
";
        let parsed = parse_accounts(raw).expect("valid accounts");
        assert_eq!(parsed.addresses.len(), 2);
        assert_eq!(parsed.private_keys.len(), 2);
    }

    #[test]
    fn parse_accounts_rejects_wrong_token_count() {
        let raw = "0x1111111111111111111111111111111111111111";
        let err = parse_accounts(raw).expect_err("missing key must fail");
        assert!(err.contains("expected exactly two tokens") || err.contains("missing private key"));
    }

    #[test]
    fn parse_accounts_rejects_wrong_lengths_and_non_hex() {
        let wrong_len = "0x111111111111111111111111111111111111111 0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let err = parse_accounts(wrong_len).expect_err("wrong address len must fail");
        assert!(err.contains("invalid address length"));

        let non_hex = "0x1111111111111111111111111111111111111111 0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
        let err = parse_accounts(non_hex).expect_err("non-hex key must fail");
        assert!(err.contains("private key must be hex"));
    }
}
