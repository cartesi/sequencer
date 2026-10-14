// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Canonical SSZ encoding of the toy wallet's state (layout owned by
//! `docs/snapshots/format.md`). [`encode`] and [`decode`] are the only codec:
//! `WalletApp::create_dump`, CM `inspect_state` and the watchdog digest use them.
//! Golden bytes: `tests/fixtures/wallet_snapshot_empty.hex`, shared with Lua tests.

use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};
use ssz::{Decode, Encode};
use ssz_derive::{Decode as SszDecode, Encode as SszEncode};

use crate::application::{WalletApp, WalletConfig};
use sequencer_core::application::{AppError, Application};

#[derive(Debug, Clone, PartialEq, Eq, SszEncode, SszDecode)]
struct SnapshotBalance {
    pub address: [u8; 20],
    pub balance_be: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, SszEncode, SszDecode)]
struct SnapshotNonce {
    pub address: [u8; 20],
    pub nonce: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, SszEncode, SszDecode)]
struct WalletSnapshot {
    pub erc20_portal_address: [u8; 20],
    pub supported_erc20_token: [u8; 20],
    pub sequencer_address: [u8; 20],
    pub balances: Vec<SnapshotBalance>,
    pub nonces: Vec<SnapshotNonce>,
    pub executed_input_count: u64,
    pub last_executed_safe_block: u64,
}

/// Deterministic SSZ bytes for `app`'s logical state; entries come out in
/// ascending address order (`BTreeMap` iteration order).
pub fn encode(app: &WalletApp) -> Vec<u8> {
    let balances: Vec<_> = app
        .balances_iter()
        .map(|(address, balance)| SnapshotBalance {
            address: address.into_array(),
            balance_be: balance.to_be_bytes(),
        })
        .collect();

    let nonces: Vec<_> = app
        .nonces_iter()
        .map(|(address, nonce)| SnapshotNonce {
            address: address.into_array(),
            nonce: *nonce,
        })
        .collect();

    WalletSnapshot {
        erc20_portal_address: app.config().erc20_portal_address.into_array(),
        supported_erc20_token: app.config().supported_erc20_token.into_array(),
        sequencer_address: app.config().sequencer_address.into_array(),
        balances,
        nonces,
        executed_input_count: app.executed_input_count().get(),
        last_executed_safe_block: app.last_executed_safe_block(),
    }
    .as_ssz_bytes()
}

/// Rehydrate a [`WalletApp`] from SSZ snapshot bytes.
pub fn decode(bytes: &[u8]) -> Result<WalletApp, AppError> {
    let decoded = WalletSnapshot::from_ssz_bytes(bytes).map_err(|e| AppError::Internal {
        reason: format!("snapshot decode failed: {e:?}"),
    })?;

    let mut balances = BTreeMap::new();
    for entry in decoded.balances {
        let address = Address::from(entry.address);
        let balance = U256::from_be_bytes(entry.balance_be);
        if balances.insert(address, balance).is_some() {
            return Err(AppError::Internal {
                reason: format!("snapshot contains duplicate balance entry for {address}"),
            });
        }
    }

    let mut nonces = BTreeMap::new();
    for entry in decoded.nonces {
        let address = Address::from(entry.address);
        if nonces.insert(address, entry.nonce).is_some() {
            return Err(AppError::Internal {
                reason: format!("snapshot contains duplicate nonce entry for {address}"),
            });
        }
    }

    WalletApp::from_snapshot_parts(
        WalletConfig {
            erc20_portal_address: Address::from(decoded.erc20_portal_address),
            supported_erc20_token: Address::from(decoded.supported_erc20_token),
            sequencer_address: Address::from(decoded.sequencer_address),
        },
        balances,
        nonces,
        decoded.executed_input_count,
        decoded.last_executed_safe_block,
    )
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
    }

    fn read_fixture_hex(name: &str) -> Vec<u8> {
        let hex = std::fs::read_to_string(fixture_path(name))
            .unwrap_or_else(|e| panic!("read fixture {name}: {e}"));
        let hex = hex.trim().replace(['\n', ' '], "");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex nibble"))
            .collect()
    }

    #[test]
    fn encode_default_wallet_matches_golden_vector() {
        let app = WalletApp::new(WalletConfig::default());
        assert_eq!(encode(&app), read_fixture_hex("wallet_snapshot_empty.hex"));
    }

    #[test]
    fn encode_round_trips_through_decode() {
        let mut app = WalletApp::new(WalletConfig::default());
        app.balances_mut().insert(
            address!("0x1111111111111111111111111111111111111111"),
            U256::from(10_u64),
        );
        app.nonces_mut()
            .insert(address!("0x1111111111111111111111111111111111111111"), 1);
        app.set_executed_input_count(3);

        let bytes = encode(&app);
        let restored = decode(&bytes).expect("decode snapshot");
        assert_eq!(encode(&restored), bytes);
    }

    #[test]
    fn encode_emits_entries_in_ascending_address_order() {
        // These rank differently under byte order, little-endian chunks and
        // reversed bytes, so only byte-lexicographic order passes.
        let first_byte = address!("0x0100000000000000000000000000000000000000");
        let second_byte = address!("0x0002000000000000000000000000000000000000");
        let last_byte = address!("0x00000000000000000000000000000000000000ff");
        let tail_chunk = address!("0x0000000000000000000000000000000000000100");
        let config = WalletConfig::sepolia();
        let mut app = WalletApp::new(config);
        for (address, balance) in [
            (first_byte, 1_u64),
            (last_byte, 2),
            (second_byte, 3),
            (tail_chunk, 4),
        ] {
            app.balances_mut().insert(address, U256::from(balance));
        }
        app.nonces_mut().insert(first_byte, 7);
        app.nonces_mut().insert(last_byte, 8);

        let balance = |address: Address, value: u64| SnapshotBalance {
            address: address.into_array(),
            balance_be: U256::from(value).to_be_bytes(),
        };
        let nonce = |address: Address, nonce: u32| SnapshotNonce {
            address: address.into_array(),
            nonce,
        };
        let expected = WalletSnapshot {
            erc20_portal_address: config.erc20_portal_address.into_array(),
            supported_erc20_token: config.supported_erc20_token.into_array(),
            sequencer_address: config.sequencer_address.into_array(),
            balances: vec![
                balance(last_byte, 2),
                balance(tail_chunk, 4),
                balance(second_byte, 3),
                balance(first_byte, 1),
            ],
            nonces: vec![nonce(last_byte, 8), nonce(first_byte, 7)],
            executed_input_count: 0,
            last_executed_safe_block: 0,
        };
        assert_eq!(encode(&app), expected.as_ssz_bytes());
    }
}
