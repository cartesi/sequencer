// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Foreclosure is irreversible: refuse admission and further batch submission.

use alloy::providers::Provider;
use alloy::rpc::types::BlockNumberOrTag;
use alloy_primitives::Address;
use cartesi_rollups_contracts::i_application::IApplication;

#[derive(Debug, thiserror::Error)]
pub enum ApplicationStatusError {
    #[error("application {app} is foreclosed; automatic recovery cannot resume service")]
    Foreclosed { app: Address },
    #[error("application status provider/transport: {0}")]
    Provider(String),
    #[error("invalid application status response: {0}")]
    InvalidResponse(String),
    #[error("application status RPC chain id {rpc} does not match pinned chain id {expected}")]
    ChainIdMismatch { rpc: u64, expected: u64 },
}

impl ApplicationStatusError {
    pub(crate) fn is_terminal(&self) -> bool {
        match self {
            Self::Foreclosed { .. } | Self::ChainIdMismatch { .. } | Self::InvalidResponse(_) => {
                true
            }
            Self::Provider(_) => false,
        }
    }
}

pub(crate) async fn require_open(
    provider: &impl Provider,
    app: Address,
    expected_chain_id: u64,
) -> Result<(), ApplicationStatusError> {
    let rpc = provider
        .get_chain_id()
        .await
        .map_err(|error| ApplicationStatusError::Provider(error.to_string()))?;
    if rpc != expected_chain_id {
        return Err(ApplicationStatusError::ChainIdMismatch {
            rpc,
            expected: expected_chain_id,
        });
    }
    // Latest detects closure before safe finality. A transient reorg may cause
    // a conservative stop; a deliberate restart rechecks the contract.
    let foreclosed = IApplication::new(app, provider)
        .isForeclosed()
        .block(BlockNumberOrTag::Latest.into())
        .call()
        .await
        .map_err(|error| match error {
            alloy::contract::Error::TransportError(_) => {
                ApplicationStatusError::Provider(error.to_string())
            }
            _ => ApplicationStatusError::InvalidResponse(error.to_string()),
        })?;
    if foreclosed {
        return Err(ApplicationStatusError::Foreclosed { app });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;
    use alloy_primitives::{Bytes, U64};

    #[tokio::test]
    async fn status_distinguishes_unavailable_rpc_from_invalid_contract_response() {
        for malformed in [false, true] {
            let mock = Asserter::new();
            mock.push_success(&U64::from(31337));
            if malformed {
                mock.push_success(&Bytes::new());
            } else {
                mock.push_failure_msg("temporarily unavailable");
            }
            let provider = ProviderBuilder::new().connect_mocked_client(mock);
            let error = require_open(&provider, Address::repeat_byte(1), 31337)
                .await
                .unwrap_err();
            assert_eq!(error.is_terminal(), malformed);
        }
    }

    #[tokio::test]
    async fn status_requires_the_right_chain_and_an_open_application() {
        let app = Address::repeat_byte(1);
        for foreclosed in [false, true] {
            let mock = Asserter::new();
            mock.push_success(&U64::from(31337));
            let mut result = vec![0; 32];
            result[31] = u8::from(foreclosed);
            mock.push_success(&Bytes::from(result));
            let provider = ProviderBuilder::new().connect_mocked_client(mock);
            let result = require_open(&provider, app, 31337).await;
            if foreclosed {
                assert!(
                    matches!(result, Err(ApplicationStatusError::Foreclosed { app: a }) if a == app)
                );
            } else {
                result.unwrap();
            }
        }
        let mock = Asserter::new();
        mock.push_success(&U64::from(1));
        let provider = ProviderBuilder::new().connect_mocked_client(mock);
        assert!(matches!(
            require_open(&provider, app, 31337).await,
            Err(ApplicationStatusError::ChainIdMismatch {
                rpc: 1,
                expected: 31337
            })
        ));
    }
}
