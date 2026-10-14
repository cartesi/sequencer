// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Test-only signing helpers shared across crates. Compiled for this crate's
//! tests and the `test-support` feature, which the guest build never enables.

use alloy_primitives::{B256, Signature};
use alloy_sol_types::{Eip712Domain, SolStruct};
use k256::ecdsa::SigningKey;

use crate::user_op::UserOp;

/// Low-s signature of a 32-byte prehash with the parity that recovers `key`'s address.
pub fn sign_hash(key: &SigningKey, hash: &B256) -> Signature {
    let (signature, recovery_id) = key
        .sign_prehash_recoverable(hash.as_slice())
        .expect("a 32-byte prehash always signs");
    Signature::from_signature_and_parity(signature, recovery_id.is_y_odd())
}

/// Sign `user_op` as a wallet would: EIP-712 under `domain`.
pub fn sign_user_op(key: &SigningKey, domain: &Eip712Domain, user_op: &UserOp) -> Signature {
    sign_hash(key, &user_op.eip712_signing_hash(domain))
}
