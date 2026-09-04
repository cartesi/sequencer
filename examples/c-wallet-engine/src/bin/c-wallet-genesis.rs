// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Writes a genesis state for the wallet engine. The seam has no create path, so every
//! application ships a tool like this, and the host never learns what it configured.

use std::path::PathBuf;
use std::process::ExitCode;

use app_core::application::WalletConfig;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let config = match arguments.as_slice() {
        [_, preset] if preset == "devnet" => WalletConfig::devnet(),
        [_, preset] if preset == "sepolia" => WalletConfig::sepolia(),
        [_] => WalletConfig::default(),
        _ => {
            eprintln!(
                "usage: c-wallet-genesis <state-dir> [devnet|sepolia]\n\n\
                 Writes a genesis wallet state at <state-dir>, which must not already exist."
            );
            return ExitCode::from(2);
        }
    };

    let state_dir = PathBuf::from(&arguments[0]);
    if let Err(err) = c_wallet_engine::write_genesis(&state_dir, config) {
        eprintln!("cannot write {}: {err:?}", state_dir.display());
        return ExitCode::FAILURE;
    }
    println!("wrote {}", state_dir.display());
    ExitCode::SUCCESS
}
