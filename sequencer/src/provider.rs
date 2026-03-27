// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::str::FromStr;
use std::time::Duration;

use alloy::{
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::client::RpcClient,
    signers::local::PrivateKeySigner,
    transports::http::{reqwest, Http, reqwest::Url},
};
use alloy_transport::layers::RetryBackoffLayer;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RATE_LIMIT_RETRIES: u32 = 5;
const INITIAL_BACKOFF_MS: u64 = 200;
const COMPUTE_UNITS_PER_SEC: u64 = 500;

fn create_client(url: &str) -> Result<RpcClient, String> {
    let url = Url::parse(url).map_err(|e| format!("invalid RPC URL: {e}"))?;

    let http_client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let transport = Http::with_client(http_client, url);
    let is_local = transport.guess_local();

    let retry = RetryBackoffLayer::new(
        MAX_RATE_LIMIT_RETRIES,
        INITIAL_BACKOFF_MS,
        COMPUTE_UNITS_PER_SEC,
    );

    Ok(RpcClient::builder()
        .layer(retry)
        .transport(transport, is_local))
}

/// Create a read-only provider with retry and timeout.
pub fn create_provider(url: &str) -> Result<DynProvider, String> {
    let client = create_client(url)?;
    let provider = ProviderBuilder::new().connect_client(client);
    Ok(provider.erased())
}

/// Create a provider with a wallet signer, retry, and timeout.
pub fn create_signer_provider(url: &str, private_key: &str) -> Result<DynProvider, String> {
    let client = create_client(url)?;
    let signer =
        PrivateKeySigner::from_str(private_key).map_err(|e| format!("invalid private key: {e}"))?;
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_client(client);
    Ok(provider.erased())
}
