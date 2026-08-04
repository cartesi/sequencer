// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::Duration;

use alloy::network::EthereumWallet;
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy_primitives::{Address, Signature, U256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use app_core::application::{Method, Transfer, Withdrawal, default_private_keys};
use cartesi_rollups_contracts::erc20_portal::ERC20Portal;
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::hazmat::PrehashSigner;
use sequencer_core::api::{TxRequest, TxResponse};
use sequencer_core::user_op::UserOp;
use sequencer_rust_client::SequencerClient;

use crate::HarnessResult;
use crate::util::io_other;

const DEFAULT_SEQUENCER_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default max_fee for harness-submitted transactions.
/// Must exceed the default log_recommended_fee (1356) and live oracle prices.
const DEFAULT_MAX_FEE: u16 = 2500;

sol! {
    #[sol(rpc)]
    interface MockERC20 {
        function mint(address to, uint256 value) external;
        function approve(address spender, uint256 value) external returns (bool);
    }
}

#[derive(Clone)]
pub struct TestSigner {
    signing_key: SigningKey,
    address: Address,
}

impl TestSigner {
    pub fn from_default(index: usize) -> HarnessResult<Self> {
        let private_key = default_private_keys()
            .get(index)
            .ok_or_else(|| io_other(format!("missing default private key at index {index}")))?;
        Self::from_private_key_hex(private_key)
    }

    pub fn from_private_key_hex(private_key: &str) -> HarnessResult<Self> {
        let signing_key = decode_signing_key(private_key)?;
        let address = address_from_signing_key(&signing_key);
        Ok(Self {
            signing_key,
            address,
        })
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    pub(crate) fn signed_request(
        &self,
        domain: &Eip712Domain,
        message: UserOp,
    ) -> HarnessResult<TxRequest> {
        Ok(TxRequest {
            signature: self.sign_user_op_hex(domain, &message)?,
            sender: self.address.to_string(),
            message,
        })
    }

    pub(crate) async fn connect_l1_provider(&self, endpoint: &str) -> HarnessResult<DynProvider> {
        let signer = PrivateKeySigner::from(self.signing_key.clone());
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect(endpoint)
            .await
            .map_err(|err| io_other(format!("failed to connect L1 wallet provider: {err}")))?;
        Ok(provider.erased())
    }

    fn sign_user_op_hex(&self, domain: &Eip712Domain, user_op: &UserOp) -> HarnessResult<String> {
        sign_user_op_hex(&self.signing_key, domain, user_op)
    }
}

pub struct WalletL1Client {
    signer: TestSigner,
    provider: DynProvider,
    app_address: Address,
    erc20_portal_address: Address,
    supported_erc20_token: Address,
}

impl WalletL1Client {
    pub(crate) async fn connect(
        endpoint: &str,
        app_address: Address,
        erc20_portal_address: Address,
        supported_erc20_token: Address,
        signer: TestSigner,
    ) -> HarnessResult<Self> {
        let provider = signer.connect_l1_provider(endpoint).await?;
        Ok(Self {
            signer,
            provider,
            app_address,
            erc20_portal_address,
            supported_erc20_token,
        })
    }

    pub async fn mint_token(&self, token_address: Address, amount: U256) -> HarnessResult<()> {
        let token = MockERC20::new(token_address, &self.provider);
        let receipt = token
            .mint(self.signer.address(), amount)
            .send()
            .await
            .map_err(|err| io_other(format!("failed to send mock ERC20 mint transaction: {err}")))?
            .get_receipt()
            .await
            .map_err(|err| {
                io_other(format!(
                    "failed to confirm mock ERC20 mint transaction: {err}"
                ))
            })?;
        ensure_success(receipt.status(), "mock ERC20 mint")
    }

    pub async fn mint_supported_token(&self, amount: U256) -> HarnessResult<()> {
        self.mint_token(self.supported_erc20_token, amount).await
    }

    pub async fn send_eth(&self, recipient: Address, amount: U256) -> HarnessResult<()> {
        let receipt = self
            .provider
            .send_transaction(TransactionRequest::default().to(recipient).value(amount))
            .await
            .map_err(|err| io_other(format!("failed to send L1 funding transaction: {err}")))?
            .get_receipt()
            .await
            .map_err(|err| io_other(format!("failed to confirm L1 funding transaction: {err}")))?;
        ensure_success(receipt.status(), "L1 funding transaction")
    }

    pub async fn mint_and_deposit_token(
        &self,
        token_address: Address,
        amount: U256,
    ) -> HarnessResult<()> {
        self.mint_token(token_address, amount).await?;
        self.deposit_token(token_address, amount).await
    }

    pub async fn mint_and_deposit_supported_token(&self, amount: U256) -> HarnessResult<()> {
        self.mint_supported_token(amount).await?;
        self.deposit_supported_token(amount).await
    }

    pub async fn deposit_token(&self, token_address: Address, amount: U256) -> HarnessResult<()> {
        let token = MockERC20::new(token_address, &self.provider);
        let approve_receipt = token
            .approve(self.erc20_portal_address, amount)
            .send()
            .await
            .map_err(|err| {
                io_other(format!(
                    "failed to send mock ERC20 approve transaction: {err}"
                ))
            })?
            .get_receipt()
            .await
            .map_err(|err| {
                io_other(format!(
                    "failed to confirm mock ERC20 approve transaction: {err}"
                ))
            })?;
        ensure_success(approve_receipt.status(), "mock ERC20 approve")?;

        let portal = ERC20Portal::new(self.erc20_portal_address, &self.provider);
        let deposit_receipt = portal
            .depositERC20Tokens(
                token_address,
                self.app_address,
                amount,
                alloy_primitives::Bytes::default(),
            )
            .send()
            .await
            .map_err(|err| {
                io_other(format!(
                    "failed to send ERC20 portal deposit transaction: {err}"
                ))
            })?
            .get_receipt()
            .await
            .map_err(|err| {
                io_other(format!(
                    "failed to confirm ERC20 portal deposit transaction: {err}"
                ))
            })?;
        ensure_success(deposit_receipt.status(), "ERC20 portal deposit")
    }

    pub async fn deposit_supported_token(&self, amount: U256) -> HarnessResult<()> {
        self.deposit_token(self.supported_erc20_token, amount).await
    }
}

pub struct WalletL2Client {
    signer: TestSigner,
    client: SequencerClient,
    domain: Eip712Domain,
    next_nonce: u32,
}

impl WalletL2Client {
    pub(crate) fn new(
        endpoint: &str,
        chain_id: u64,
        verifying_contract: Address,
        signer: TestSigner,
    ) -> HarnessResult<Self> {
        let client = SequencerClient::new_with_timeout(
            endpoint.to_string(),
            DEFAULT_SEQUENCER_CLIENT_TIMEOUT,
        )?;
        let domain = sequencer_core::build_input_domain(chain_id, verifying_contract);
        Ok(Self {
            signer,
            client,
            domain,
            next_nonce: 0,
        })
    }

    /// Override the next user-op nonce this client will sign. A fresh client
    /// starts at 0; after a state-preserving recovery (where the sender's nonce
    /// carries over) a test sets this to the sender's continuing nonce so its
    /// post-recovery ops are accepted.
    pub fn set_next_nonce(&mut self, nonce: u32) {
        self.next_nonce = nonce;
    }

    pub async fn transfer(&mut self, to: Address, amount: U256) -> HarnessResult<TxResponse> {
        self.submit_method(Method::Transfer(Transfer { amount, to }))
            .await
    }

    pub async fn withdraw(&mut self, amount: U256) -> HarnessResult<TxResponse> {
        self.submit_method(Method::Withdrawal(Withdrawal { amount }))
            .await
    }

    async fn submit_method(&mut self, method: Method) -> HarnessResult<TxResponse> {
        let expected_nonce = self.next_nonce;
        let request = self.signer.signed_request(
            &self.domain,
            UserOp {
                nonce: expected_nonce,
                max_fee: DEFAULT_MAX_FEE,
                data: ssz::Encode::as_ssz_bytes(&method).into(),
            },
        )?;
        let response = self.client.submit_tx(&request).await?;

        if !response.ok {
            return Err(io_other(format!(
                "sequencer rejected tx from {} with nonce {}",
                self.signer.address(),
                expected_nonce
            ))
            .into());
        }
        if response.sender != self.signer.address().to_string() {
            return Err(io_other(format!(
                "sequencer response sender mismatch: expected {}, got {}",
                self.signer.address(),
                response.sender
            ))
            .into());
        }
        if response.nonce != expected_nonce {
            return Err(io_other(format!(
                "sequencer response nonce mismatch: expected {}, got {}",
                expected_nonce, response.nonce
            ))
            .into());
        }

        self.next_nonce = self
            .next_nonce
            .checked_add(1)
            .ok_or_else(|| io_other("wallet L2 nonce overflow"))?;

        Ok(response)
    }
}

pub fn sign_user_op_hex(
    signing_key: &SigningKey,
    domain: &Eip712Domain,
    user_op: &UserOp,
) -> HarnessResult<String> {
    let hash = user_op.eip712_signing_hash(domain);
    let k256_sig = signing_key
        .sign_prehash(hash.as_slice())
        .map_err(|err| io_other(format!("failed to sign user op hash: {err}")))?;

    let expected_sender = address_from_signing_key(signing_key);
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|candidate| {
            candidate
                .recover_address_from_prehash(&hash)
                .ok()
                .map(|value| value == expected_sender)
                .unwrap_or(false)
        })
        .ok_or_else(|| io_other("failed to derive recoverable signature parity"))?;

    Ok(alloy_primitives::hex::encode_prefixed(signature.as_bytes()))
}

fn decode_signing_key(private_key_hex: &str) -> HarnessResult<SigningKey> {
    let private_key_bytes = alloy_primitives::hex::decode(
        private_key_hex
            .strip_prefix("0x")
            .unwrap_or(private_key_hex),
    )?;
    Ok(SigningKey::from_bytes(private_key_bytes.as_slice().into())?)
}

pub fn address_from_signing_key(signing_key: &SigningKey) -> Address {
    let verifying = signing_key.verifying_key().to_encoded_point(false);
    Address::from_raw_public_key(&verifying.as_bytes()[1..])
}

fn ensure_success(status: bool, label: &str) -> HarnessResult<()> {
    if status {
        Ok(())
    } else {
        Err(io_other(format!("{label} reverted")).into())
    }
}
