// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Anvil's default funded accounts, `<address> <private_key>` per line, in
//! account-index order.

use std::sync::OnceLock;

const ANVIL_ACCOUNTS: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/anvil_accounts.txt"));

/// Private keys of Anvil's default accounts; index `i` is Anvil account `i`.
pub fn default_private_keys() -> &'static [String] {
    static KEYS: OnceLock<Vec<String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        ANVIL_ACCOUNTS
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                line.split_whitespace()
                    .nth(1)
                    .expect("anvil_accounts.txt: `<address> <private_key>` per line")
                    .to_owned()
            })
            .collect()
    })
}
