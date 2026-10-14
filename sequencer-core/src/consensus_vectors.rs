// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Byte-level consensus vectors from `tests/fixtures/consensus_vectors_v1.json`.
//! They were pinned once, before any toolchain or dependency move, and are never
//! regenerated: a mismatch means consensus bytes changed and is a finding.
//! Signature, SSZ and fee cases are asserted next to the code they pin.

use alloy_primitives::{Address, B256, Bytes, U64, U256, keccak256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::batch::{Batch, Frame, WireUserOp};
use crate::user_op::UserOp;

const CONSENSUS_VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tests/fixtures/consensus_vectors_v1.json"
));

pub(crate) fn vectors() -> ConsensusVectors {
    serde_json::from_str(CONSENSUS_VECTORS).expect("parse consensus_vectors_v1.json")
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConsensusVectors {
    pub eip712: Eip712Vectors,
    pub keccak256: Vec<KeccakCase>,
    pub signature: SignatureVectors,
    pub ssz: SszVectors,
    pub fee: FeeVectors,
    pub guest: GuestPayloads,
}

impl ConsensusVectors {
    /// The pinned domain named `name`, built through the production constructor.
    pub fn domain(&self, name: &str) -> Eip712Domain {
        let pinned = self
            .eip712
            .domains
            .iter()
            .find(|domain| domain.name == name)
            .unwrap_or_else(|| panic!("no pinned domain {name}"));
        crate::build_input_domain(pinned.chain_id, pinned.verifying_contract)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Eip712Vectors {
    pub domain_encode_type: String,
    pub domain_type_hash: B256,
    pub encode_type: String,
    pub type_hash: B256,
    pub domains: Vec<DomainCase>,
    pub user_ops: Vec<UserOpCase>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DomainCase {
    pub name: String,
    pub chain_id: u64,
    pub verifying_contract: Address,
    pub separator: B256,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UserOpCase {
    pub name: String,
    pub domain: String,
    #[serde(flatten)]
    pub user_op: UserOp,
    pub struct_hash: B256,
    pub signing_hash: B256,
}

#[derive(Debug, Deserialize)]
pub(crate) struct KeccakCase {
    pub name: String,
    pub input: Bytes,
    pub digest: B256,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SignatureVectors {
    pub private_key: B256,
    pub address: Address,
    pub domain: String,
    pub user_op: UserOp,
    pub signature: Bytes,
    pub variants: Vec<SignatureCase>,
}

/// One signature variant and what each path makes of it today: the recovered
/// sender, or `None` when the path refuses the signature.
#[derive(Debug, Deserialize)]
pub(crate) struct SignatureCase {
    pub name: String,
    pub signature: Bytes,
    pub guest: Option<Address>,
    pub ingress: Option<Address>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SszVectors {
    pub spill_batch: PinnedBatch,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PinnedBatch {
    pub nonce: u64,
    pub frames: Vec<PinnedFrame>,
    pub encoded: Bytes,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PinnedFrame {
    pub safe_block: u64,
    pub fee_price: u16,
    pub user_ops: Vec<PinnedWireUserOp>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PinnedWireUserOp {
    pub nonce: u32,
    pub max_fee: u16,
    pub data: Bytes,
    pub signature: Bytes,
}

impl PinnedBatch {
    pub fn to_batch(&self) -> Batch {
        Batch {
            nonce: self.nonce,
            frames: self
                .frames
                .iter()
                .map(|frame| Frame {
                    user_ops: frame
                        .user_ops
                        .iter()
                        .map(|op| WireUserOp {
                            nonce: op.nonce,
                            max_fee: op.max_fee,
                            data: op.data.to_vec(),
                            signature: op.signature.to_vec(),
                        })
                        .collect(),
                    safe_block: frame.safe_block,
                    fee_price: frame.fee_price,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct FeeVectors {
    pub max_exponent: u16,
    /// `TABLE[i]` as its four little-endian `u64` limbs.
    pub table: Vec<[U64; 4]>,
    pub fee_to_linear: Vec<FeeCase>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FeeCase {
    pub exponent: u16,
    /// The fixed-point accumulator before the final shift.
    pub fixed: U256,
    pub linear: U256,
}

/// The guest scenarios' input payloads; `canonical-test` replays them.
#[derive(Debug, Deserialize)]
pub(crate) struct GuestPayloads {
    pub payloads: BTreeMap<String, Bytes>,
}

#[test]
fn eip712_types_and_domain_separators_match_the_pinned_bytes() {
    let vectors = vectors();
    let pinned = &vectors.eip712;
    assert_eq!(UserOp::eip712_encode_type(), pinned.encode_type);
    let probe = UserOp {
        nonce: 0,
        max_fee: 0,
        data: Bytes::new(),
    };
    assert_eq!(probe.eip712_type_hash(), pinned.type_hash);
    assert_eq!(pinned.type_hash, keccak256(pinned.encode_type.as_bytes()));

    for case in &pinned.domains {
        let domain = vectors.domain(&case.name);
        assert_eq!(
            domain.encode_type(),
            pinned.domain_encode_type,
            "{}",
            case.name
        );
        assert_eq!(domain.type_hash(), pinned.domain_type_hash, "{}", case.name);
        assert_eq!(domain.separator(), case.separator, "{}", case.name);
    }
}

#[test]
fn eip712_signing_hashes_match_the_pinned_bytes() {
    let vectors = vectors();
    for case in &vectors.eip712.user_ops {
        let domain = vectors.domain(&case.domain);
        assert_eq!(
            case.user_op.eip712_hash_struct(),
            case.struct_hash,
            "{}",
            case.name
        );
        assert_eq!(
            case.user_op.eip712_signing_hash(&domain),
            case.signing_hash,
            "{}",
            case.name
        );
    }
}

/// The guest hashes through `alloy_primitives::keccak256`; the lengths straddle
/// the 136-byte sponge rate.
#[test]
fn keccak256_matches_the_pinned_digests() {
    for case in vectors().keccak256 {
        assert_eq!(keccak256(&case.input), case.digest, "{}", case.name);
    }
}

/// RFC 6979 makes the signature a function of key and hash, so the signing
/// helpers reproduce the pinned bytes the variant tables start from.
#[test]
fn signing_reproduces_the_pinned_signature() {
    let vectors = vectors();
    let pinned = &vectors.signature;
    let key = k256::ecdsa::SigningKey::from_slice(pinned.private_key.as_slice())
        .expect("pinned private key");
    let encoded_point = key.verifying_key().to_encoded_point(false);
    assert_eq!(
        Address::from_raw_public_key(&encoded_point.as_bytes()[1..]),
        pinned.address
    );

    let domain = vectors.domain(&pinned.domain);
    let signature = crate::test_support::sign_user_op(&key, &domain, &pinned.user_op);
    assert_eq!(signature.as_bytes().as_slice(), pinned.signature.as_ref());
    assert_eq!(
        signature
            .recover_address_from_prehash(&pinned.user_op.eip712_signing_hash(&domain))
            .expect("recover the pinned signer"),
        pinned.address
    );
}
