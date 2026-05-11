// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::io::ErrorKind;
use std::time::Duration;

use alloy_primitives::{Address, Signature, U256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use app_core::application::{
    MAX_METHOD_PAYLOAD_BYTES, Method, WalletApp, WalletConfig, Withdrawal,
};
use futures_util::StreamExt;
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::hazmat::PrehashSigner;
use sequencer::egress::l2_tx_feed::{L2TxFeed, L2TxFeedConfig};
use sequencer::http::{self, ApiConfig};
use sequencer::ingress::inclusion_lane::{
    InclusionLane, InclusionLaneConfig, InclusionLaneError, PendingUserOp,
};
use sequencer::runtime::shutdown::ShutdownSignal;
use sequencer::storage::{SafeInputRange, Storage, StoredSafeInput};
use sequencer_core::api::{TxRequest, TxResponse, WsTxMessage};
use sequencer_core::l2_tx::SequencedL2Tx;
use sequencer_core::user_op::UserOp;
use sequencer_rust_client::SequencerClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

mod common;
use common::temp_db;

// ── V1 regression: cross-boundary signature domain consistency ────────
//
// The sequencer signs user-ops with `sequencer_core::build_input_domain`. The
// scheduler (canonical-app) recovers senders with the same function. If the
// two sides ever drift (the V1 bug: scheduler had `name: None`, sequencer had
// `name: Some("CartesiAppSequencer")`), every signature recovers a different
// address on each side, structurally breaking the rollup.
//
// These tests lock the invariant at two levels:
//   1. A signature built via the shared constructor recovers the signer's
//      address (positive).
//   2. A signature built with ANY domain that differs from the shared
//      constructor recovers a DIFFERENT address (negative — proves the domain
//      actually affects recovery).

#[test]
fn v1_regression_shared_domain_recovers_signer() {
    use alloy_sol_types::SolStruct;

    let signing_key = SigningKey::from_bytes((&[42_u8; 32]).into()).expect("signing key");
    let signer_address = address_from_signing_key(&signing_key);

    let chain_id = 31_337_u64;
    let app = Address::from_slice(&[0xaa; 20]);
    let domain = sequencer_core::build_input_domain(chain_id, app);

    let user_op = UserOp {
        nonce: 0,
        max_fee: 1_200,
        data: vec![0x01, 0x02, 0x03].into(),
    };

    // Sign with the shared domain.
    let hash = user_op.eip712_signing_hash(&domain);
    let k256_sig = signing_key.sign_prehash(hash.as_slice()).expect("sign");
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|s| {
            s.recover_address_from_prehash(&hash)
                .ok()
                .is_some_and(|r| r == signer_address)
        })
        .expect("recoverable parity");

    // Recover with the shared domain — must equal signer.
    let hash_again = user_op.eip712_signing_hash(&domain);
    let recovered = signature
        .recover_address_from_prehash(&hash_again)
        .expect("recover");
    assert_eq!(
        recovered, signer_address,
        "shared domain must recover signer"
    );
}

#[test]
fn v1_regression_name_none_domain_recovers_different_address() {
    use alloy_sol_types::{Eip712Domain, SolStruct};

    let signing_key = SigningKey::from_bytes((&[42_u8; 32]).into()).expect("signing key");
    let signer_address = address_from_signing_key(&signing_key);

    let chain_id = 31_337_u64;
    let app = Address::from_slice(&[0xaa; 20]);
    let correct_domain = sequencer_core::build_input_domain(chain_id, app);

    // The exact buggy domain the scheduler used pre-V1 fix.
    let buggy_domain = Eip712Domain {
        name: None,
        version: None,
        chain_id: Some(U256::from(chain_id)),
        verifying_contract: Some(app),
        salt: None,
    };

    let user_op = UserOp {
        nonce: 0,
        max_fee: 1_200,
        data: vec![0x01, 0x02, 0x03].into(),
    };

    // Sign with the correct (shared) domain.
    let hash = user_op.eip712_signing_hash(&correct_domain);
    let k256_sig = signing_key.sign_prehash(hash.as_slice()).expect("sign");
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|s| {
            s.recover_address_from_prehash(&hash)
                .ok()
                .is_some_and(|r| r == signer_address)
        })
        .expect("recoverable parity");

    // Recover with the buggy domain — must NOT recover the signer.
    // (This is what would silently fail at the scheduler under the V1 bug.)
    let buggy_hash = user_op.eip712_signing_hash(&buggy_domain);
    let recovered_under_buggy = signature
        .recover_address_from_prehash(&buggy_hash)
        .expect("recovery succeeds but returns the wrong address");
    assert_ne!(
        recovered_under_buggy, signer_address,
        "a name:None domain must not recover the signer — if this fails, \
         the shared domain constructor is bit-identical to the buggy one, \
         meaning the V1 fix regressed"
    );
}

#[test]
fn v1_regression_domain_fields_all_affect_recovery() {
    use alloy_sol_types::SolStruct;

    let signing_key = SigningKey::from_bytes((&[42_u8; 32]).into()).expect("signing key");
    let signer_address = address_from_signing_key(&signing_key);

    let app = Address::from_slice(&[0xaa; 20]);
    let user_op = UserOp {
        nonce: 0,
        max_fee: 1_200,
        data: vec![0x01].into(),
    };

    // Sign with chain_id = 1.
    let chain_a = sequencer_core::build_input_domain(1, app);
    let hash_a = user_op.eip712_signing_hash(&chain_a);
    let k256_sig = signing_key.sign_prehash(hash_a.as_slice()).expect("sign");
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|s| {
            s.recover_address_from_prehash(&hash_a)
                .ok()
                .is_some_and(|r| r == signer_address)
        })
        .expect("recoverable parity");

    // Cross-chain replay must fail: recover under chain_id=2 with the same app.
    let chain_b = sequencer_core::build_input_domain(2, app);
    let hash_b = user_op.eip712_signing_hash(&chain_b);
    let recovered_b = signature
        .recover_address_from_prehash(&hash_b)
        .expect("recovery returns some address");
    assert_ne!(
        recovered_b, signer_address,
        "cross-chain replay must not recover signer"
    );

    // Cross-app replay must fail: recover under same chain but different app.
    let other_app = Address::from_slice(&[0xbb; 20]);
    let chain_a_app_other = sequencer_core::build_input_domain(1, other_app);
    let hash_app_other = user_op.eip712_signing_hash(&chain_a_app_other);
    let recovered_app_other = signature
        .recover_address_from_prehash(&hash_app_other)
        .expect("recovery returns some address");
    assert_ne!(
        recovered_app_other, signer_address,
        "cross-app replay must not recover signer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_submit_tx_ack_and_broadcast() {
    let db = temp_db("full-e2e");
    let domain = test_domain();
    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    // Fund the sender so the user-op passes the balance check.
    bootstrap_open_frame_with_deposits(db.path.as_str(), &[(sender, U256::from(1_000_000_u64))]);

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint.clone(), Duration::from_secs(2))
        .expect("build sequencer client");
    let ws_url = client.ws_subscribe_url(0);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(ws_url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    // The deposit is broadcast first.
    let deposit_message = recv_ws_message(&mut ws).await;
    match deposit_message {
        WsTxMessage::DirectInput { offset, .. } => assert_eq!(offset, 1),
        other => panic!("expected deposit direct input as first WS message, got {other:?}"),
    }
    let method = Method::Withdrawal(Withdrawal {
        amount: U256::from(0_u64),
    });
    let user_op = UserOp {
        nonce: 0,
        max_fee: TEST_MAX_FEE,
        data: ssz::Encode::as_ssz_bytes(&method).into(),
    };
    let signature_hex = sign_user_op_hex(&domain, &user_op, &signing_key);

    let request_body = TxRequest {
        message: user_op,
        signature: signature_hex,
        sender: sender.to_string(),
    };

    let (status, response_body) = client
        .submit_tx_with_status(&request_body)
        .await
        .expect("submit tx");
    assert_eq!(
        status, 200,
        "submit tx should succeed: body={response_body}"
    );

    let response: TxResponse =
        serde_json::from_str(response_body.as_str()).expect("parse response");
    assert!(response.ok);
    assert_eq!(response.nonce, 0);
    assert_eq!(response.sender, sender.to_string());

    let first_message = recv_ws_message(&mut ws).await;
    match first_message {
        WsTxMessage::UserOp {
            offset,
            sender: ws_sender,
            fee,
            data,
        } => {
            assert_eq!(offset, 2);
            assert_eq!(ws_sender, sender.to_string());
            // Frame fee is the default log_recommended_fee = 1060.
            assert_eq!(fee, 1060);
            assert_eq!(
                decode_hex_prefixed(data.as_str()),
                ssz::Encode::as_ssz_bytes(&method)
            );
        }
        value => panic!("expected user_op at offset 0, got {value:?}"),
    }

    drop(ws);
    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_signature_with_wrong_hex_length() {
    let db = temp_db("bad-signature-hex-len");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    let mut request = make_valid_request(&domain);
    request.signature = "0xdeadbeef".to_string();
    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");

    assert_eq!(
        status, 400,
        "unexpected status for bad signature len: {body}"
    );
    assert!(
        body.contains("signature must be"),
        "expected signature length message, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_sender_with_wrong_hex_length() {
    let db = temp_db("bad-sender-hex-len");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    let mut request = make_valid_request(&domain);
    request.sender = "0x1234".to_string();
    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");

    assert_eq!(status, 400, "unexpected status for bad sender len: {body}");
    assert!(
        body.contains("sender must be"),
        "expected sender length message, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_oversized_json_body_before_parsing() {
    let db = temp_db("oversized-json");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server_with_max_body(db.path.as_str(), domain, 256).await else {
        return;
    };

    let oversized_json = format!(
        r#"{{"message":{{"nonce":0,"max_fee":0,"data":"0x"}},"signature":"0x{}","sender":null,"pad":"{}"}}"#,
        "00".repeat(65),
        " ".repeat(4096)
    );
    let (status, body) = post_raw_json(runtime.addr, oversized_json.as_str()).await;

    assert_eq!(
        status, 413,
        "unexpected status for oversized JSON body: {body}"
    );
    assert!(
        body.contains("PAYLOAD_TOO_LARGE"),
        "expected payload-too-large error code, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_malformed_json_as_bad_request() {
    let db = temp_db("malformed-json");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server_with_max_body(db.path.as_str(), domain, 128 * 1024).await
    else {
        return;
    };

    let malformed_json =
        r#"{"message":{"nonce":0,"max_fee":0,"data":"0x"},"signature":"0x00","sender":"0x1234""#;
    let (status, body) = post_raw_json(runtime.addr, malformed_json).await;

    assert_eq!(
        status, 400,
        "unexpected status for malformed JSON body: {body}"
    );
    assert!(
        body.contains("BAD_REQUEST"),
        "expected bad-request error code, got: {body}"
    );

    //  / H2 regression: the message must come from the fixed taxonomy
    // ("invalid JSON"), NOT reflect serde's line/column/token excerpt. The
    // malformed input contains the token `0x1234` — assert it doesn't appear
    // in the response body so no attacker-submitted bytes are echoed.
    assert!(
        body.contains("\"message\":\"invalid JSON\""),
        "expected fixed message 'invalid JSON' in body, got: {body}"
    );
    assert!(
        !body.contains("0x1234"),
        "body must not reflect attacker-submitted input bytes, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_missing_content_type_with_fixed_message() {
    //  / H2 regression: missing Content-Type must produce a fixed
    // `"missing content type"` message, not reflect any part of the request.
    let db = temp_db("missing-content-type");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server_with_max_body(db.path.as_str(), domain, 128 * 1024).await
    else {
        return;
    };

    // Valid JSON body, but sent without Content-Type: application/json.
    let (status, body) = post_raw_body_no_content_type(runtime.addr, "{}").await;
    assert_eq!(status, 400, "missing content-type: {body}");
    assert!(
        body.contains("\"message\":\"missing content type\""),
        "expected fixed 'missing content type' message, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_returns_429_when_queue_is_full() {
    let db = temp_db("queue-full-overload");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) =
        start_api_only_server(db.path.as_str(), domain.clone(), 128 * 1024, 1).await
    else {
        return;
    };

    let request = make_valid_request(&domain);
    let request_json = serde_json::to_string(&request).expect("serialize valid request");
    let first = tokio::spawn({
        let body = request_json.clone();
        let addr = runtime.addr;
        async move { post_raw_json(addr, body.as_str()).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (status, body) = post_raw_json(runtime.addr, request_json.as_str()).await;
    assert_eq!(status, 429, "expected 429 for queue-full overload: {body}");
    assert!(
        body.contains("queue full"),
        "expected queue full message in overload response: {body}"
    );
    assert!(
        body.contains("OVERLOADED"),
        "expected OVERLOADED code for queue-full overload: {body}"
    );

    first.abort();
    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_user_op_payloads_above_application_limit() {
    let db = temp_db("user-op-payload-too-large");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    let user_op = UserOp {
        nonce: 0,
        max_fee: TEST_MAX_FEE,
        data: vec![0_u8; MAX_METHOD_PAYLOAD_BYTES + 1].into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(&domain, &user_op, &signing_key),
        sender: sender.to_string(),
        message: user_op,
    };

    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit oversized tx");

    assert_eq!(
        status, 400,
        "unexpected status for oversized payload: {body}"
    );
    assert!(
        body.contains("user op payload too large"),
        "expected payload-size validation message, got: {body}"
    );
    assert!(
        body.contains(&MAX_METHOD_PAYLOAD_BYTES.to_string()),
        "expected max payload size in error body, got: {body}"
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_json_with_missing_fields_using_fixed_envelope() {
    //  / H2 regression: a body that is valid JSON but missing required
    // fields must respond with the fixed `"invalid JSON"` envelope. The
    // response must not echo serde's deserialization error text — that would
    // leak our internal field names and parser internals to callers.
    let db = temp_db("missing-fields-json");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server_with_max_body(db.path.as_str(), domain, 128 * 1024).await
    else {
        return;
    };

    // Empty object — valid JSON, missing every required field.
    let (status, body) = post_raw_json(runtime.addr, "{}").await;
    assert_eq!(status, 400, "missing fields: {body}");

    // Parse the response envelope and assert the message is exactly the fixed
    // taxonomy string. Anything else implies serde leaked internals into the
    // body — that's the regression this test pins.
    let envelope: serde_json::Value = serde_json::from_str(&body).expect("response is JSON");
    let message = envelope
        .get("message")
        .and_then(|m| m.as_str())
        .expect("envelope has string `message` field");
    assert_eq!(
        message, "invalid JSON",
        "response message must be the fixed taxonomy string, got: {message:?} (full body: {body})",
    );
    let code = envelope
        .get("code")
        .and_then(|c| c.as_str())
        .expect("envelope has string `code` field");
    assert_eq!(code, "BAD_REQUEST", "unexpected error code: {body}");

    // Sanity: serde's typical leak vocabulary must not appear anywhere.
    for needle in [
        "missing field",
        "expected",
        "deserializ",
        "line ",
        "column ",
    ] {
        assert!(
            !body.contains(needle),
            "potential serde leak — body contains {needle:?}: {body}",
        );
    }

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_payload_size_check_fires_before_signature_recovery() {
    //  sharpening: oversized `data` must be rejected by
    // `validate_payload_size` BEFORE any cryptographic work. We submit an
    // oversized payload paired with a garbage-but-correctly-shaped signature:
    // if the size check is enforced first, the response says "user op payload
    // too large"; if signature recovery ran first the response would mention a
    // signature/sender mismatch instead. Catches a regression that re-orders
    // signature verification ahead of size validation, which would open a DoS
    // vector (huge body × secp256k1 recovery cost).
    let db = temp_db("size-before-sig");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain).await else {
        return;
    };

    // Hand-craft a request: oversized data + correctly-shaped but garbage
    // signature. The 65-byte signature passes `validate_hex_lengths`, so the
    // next gate is `validate_payload_size`. If anyone moves signature recovery
    // ahead of it, the response message changes and this assertion fails.
    let oversized_data_hex = "00".repeat(MAX_METHOD_PAYLOAD_BYTES + 1);
    let bogus_sig_hex = format!("0x{}", "00".repeat(65));
    let body = format!(
        "{{\"message\":{{\"nonce\":0,\"max_fee\":0,\"data\":\"0x{oversized_data_hex}\"}},\
         \"signature\":\"{bogus_sig_hex}\",\
         \"sender\":\"0x0000000000000000000000000000000000000001\"}}",
    );
    // Confirm the body fits under the default 4 KB body limit so we exercise
    // the payload-size gate, not the upstream body-too-large gate.
    assert!(
        body.len() < 4 * 1024,
        "test body must stay under default max_body_bytes (got {} bytes)",
        body.len(),
    );

    let (status, response_body) = post_raw_json(runtime.addr, body.as_str()).await;
    assert_eq!(status, 400, "oversized + bogus sig: {response_body}");
    assert!(
        response_body.contains("user op payload too large"),
        "size check must fire before signature verification — \
         expected 'user op payload too large' message, got: {response_body}",
    );
    // Defensive: ensure the rejection is NOT a signature-class error. Any of
    // these would mean signature recovery ran on the oversized payload.
    for sig_marker in [
        "signature",
        "sender mismatch",
        "recover",
        "INVALID_SIGNATURE",
    ] {
        assert!(
            !response_body.contains(sig_marker),
            "response mentions {sig_marker:?} — signature recovery may have run \
             before the size check: {response_body}",
        );
    }

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_signature_with_invalid_parity_byte() {
    // signature with correct length (65 bytes) but a parity byte
    // outside the valid set (0/1 or 27/28) must be rejected at the crypto
    // boundary with 422. Catches regressions where a new signature codec
    // accepts arbitrary parity values and silently drifts recovery.
    let db = temp_db("bad-parity-byte");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    // Correct-length signature (65 bytes) with a non-recoverable parity byte.
    let mut bogus_sig = [0_u8; 65];
    bogus_sig[64] = 0xFF;
    let bogus_sig_hex = format!("0x{}", alloy_primitives::hex::encode(bogus_sig));

    let mut request = make_valid_request(&domain);
    request.signature = bogus_sig_hex;

    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");
    // Observed contract: 400 with `INVALID_SIGNATURE` code, same as
    // `forged_signature_rejected_test`. This test pins it.
    assert_eq!(
        status, 400,
        "invalid parity byte must produce 400 (signature-class error), got {status}: {body}",
    );
    assert!(
        body.contains("INVALID_SIGNATURE"),
        "expected INVALID_SIGNATURE code, got: {body}",
    );
    // Defensive: make sure the rejection is from the signature layer, not the
    // hex-length gate ( covers that) and not the payload-size gate.
    assert!(
        !body.contains("signature must be") && !body.contains("payload too large"),
        "expected sig-recovery class error, not hex-length or size: {body}",
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_sender_claim_that_mismatches_signature_recovery() {
    // `sender` field in the request must equal the address recovered
    // from the signature. A valid signature over a user-op paired with a
    // different claimed `sender` must be rejected — can't accept someone
    // else's signed op as if it came from ourselves. Complements the
    // integration-level forged_signature_rejected_test (which asserts the
    // end-to-end shape); this one pins the direct API response.
    let db = temp_db("sender-mismatch-explicit");
    let domain = test_domain();
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    // Key A signs the user op; we claim the sender is address B.
    let signing_key_a = SigningKey::from_bytes((&[1_u8; 32]).into()).expect("create signing key a");
    let signing_key_b = SigningKey::from_bytes((&[2_u8; 32]).into()).expect("create signing key b");
    let address_a = address_from_signing_key(&signing_key_a);
    let address_b = address_from_signing_key(&signing_key_b);
    assert_ne!(address_a, address_b, "test setup: A and B must differ");

    let user_op = UserOp {
        nonce: 0,
        max_fee: TEST_MAX_FEE,
        data: Vec::new().into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(&domain, &user_op, &signing_key_a),
        sender: address_b.to_string(),
        message: user_op,
    };

    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");
    // Observed: 400 `INVALID_SIGNATURE` `"sender mismatch"` — same
    // signature-class status as the parity-byte test above.
    assert_eq!(
        status, 400,
        "sender-mismatch must produce 400 (signature-class error), got {status}: {body}",
    );
    assert!(
        body.contains("sender mismatch"),
        "expected `sender mismatch` message, got: {body}",
    );
    assert!(
        body.contains("INVALID_SIGNATURE"),
        "expected INVALID_SIGNATURE code, got: {body}",
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_user_op_with_nonce_gap() {
    // submitting a user-op with a nonce above the next expected one
    // (i.e., a gap) must return 422 `InvalidNonce` and leave state
    // unchanged. Complement to  (nonce too low / replay) — together
    // they pin the strict-equality requirement on `current_user_nonce`.
    let db = temp_db("nonce-gap-too-high");
    let domain = test_domain();
    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    bootstrap_open_frame_with_deposits(db.path.as_str(), &[(sender, U256::from(1_000_000_u64))]);

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    // Current user nonce is 0 — a fresh sender has never submitted. Nonce 7
    // leaves a six-slot gap.
    let user_op = UserOp {
        nonce: 7,
        max_fee: TEST_MAX_FEE,
        data: ssz::Encode::as_ssz_bytes(&Method::Withdrawal(Withdrawal {
            amount: U256::from(0_u64),
        }))
        .into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(&domain, &user_op, &signing_key),
        sender: sender.to_string(),
        message: user_op,
    };

    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");
    assert_eq!(
        status, 422,
        "nonce gap must produce 422, got {status}: {body}",
    );
    assert!(
        body.contains("nonce") || body.contains("NONCE"),
        "expected nonce-class error, got: {body}",
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_accepts_user_op_with_max_fee_equal_to_current_frame_fee() {
    //  boundary: the check is `max_fee >= current_frame_fee` (strict
    // less-than rejects). An op with `max_fee == current_frame_fee` must be
    // accepted. Pairs with  (`fee_below_minimum_rejected_test`) — the
    // two together pin the comparator.
    let db = temp_db("fee-boundary-equal");
    let domain = test_domain();
    let signing_key = SigningKey::from_bytes((&[9_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    // Fund with enough to cover gas at the frame fee.
    bootstrap_open_frame_with_deposits(db.path.as_str(), &[(sender, U256::from(1_000_000_u64))]);

    // `bootstrap_open_frame` asserts frame_fee == 1060; use that exact value
    // for the boundary case.
    const FRAME_FEE_BOUNDARY: u16 = 1060;

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    let user_op = UserOp {
        nonce: 0,
        max_fee: FRAME_FEE_BOUNDARY,
        data: ssz::Encode::as_ssz_bytes(&Method::Withdrawal(Withdrawal {
            amount: U256::from(0_u64),
        }))
        .into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(&domain, &user_op, &signing_key),
        sender: sender.to_string(),
        message: user_op,
    };

    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");
    assert_eq!(
        status, 200,
        "max_fee == current_frame_fee boundary must be accepted (comparator is `<`, not `<=`), got {status}: {body}",
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_rejects_user_op_when_balance_below_gas_cost() {
    // if sender's balance < `fee_to_linear(current_frame_fee)` the
    // user op must be rejected with 422 `InsufficientGasBalance` and leave
    // state unchanged. Exercises the balance check in
    // `WalletApp::validate_user_op` (app-core). A fresh sender with no
    // deposits has balance 0, well below `fee_to_linear(1060)` (the
    // bootstrapped frame fee).
    let db = temp_db("insufficient-gas-balance");
    let domain = test_domain();
    let signing_key = SigningKey::from_bytes((&[11_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    // No deposit for `sender` → balance = 0.
    bootstrap_open_frame(db.path.as_str());

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint, Duration::from_secs(2))
        .expect("build sequencer client");

    let user_op = UserOp {
        nonce: 0,
        max_fee: TEST_MAX_FEE,
        data: ssz::Encode::as_ssz_bytes(&Method::Withdrawal(Withdrawal {
            amount: U256::from(0_u64),
        }))
        .into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(&domain, &user_op, &signing_key),
        sender: sender.to_string(),
        message: user_op,
    };

    let (status, body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");
    assert_eq!(
        status, 422,
        "insufficient-balance must produce 422, got {status}: {body}",
    );
    assert!(
        body.contains("insufficient balance for gas"),
        "expected InsufficientGasBalance message, got: {body}",
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_concurrent_same_nonce_leaves_exactly_one_committed() {
    // two concurrent POSTs for the same (sender, nonce) — one
    // succeeds, one is rejected with a nonce-class error. Pins the invariant
    // that the rejected half does NOT leave any state artifact: the final
    // balance/nonce must match the single-commit path.
    let db = temp_db("concurrent-same-nonce");
    let domain = test_domain();
    let signing_key = SigningKey::from_bytes((&[13_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    bootstrap_open_frame_with_deposits(db.path.as_str(), &[(sender, U256::from(10_000_000_u64))]);

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let user_op = UserOp {
        nonce: 0,
        max_fee: TEST_MAX_FEE,
        data: ssz::Encode::as_ssz_bytes(&Method::Withdrawal(Withdrawal {
            amount: U256::from(0_u64),
        }))
        .into(),
    };
    let request = TxRequest {
        signature: sign_user_op_hex(&domain, &user_op, &signing_key),
        sender: sender.to_string(),
        message: user_op,
    };
    let request_json = serde_json::to_string(&request).expect("serialize request");

    // Two concurrent POSTs with byte-identical bodies.
    let addr = runtime.addr;
    let body_a = request_json.clone();
    let body_b = request_json;
    let a = tokio::spawn(async move { post_raw_json(addr, body_a.as_str()).await });
    let b = tokio::spawn(async move { post_raw_json(addr, body_b.as_str()).await });
    let (res_a, res_b) = tokio::try_join!(a, b).expect("join concurrent posts");

    let outcomes = [res_a, res_b];
    let accepted = outcomes.iter().filter(|(s, _)| *s == 200).count();
    let rejected_bodies: Vec<&String> = outcomes
        .iter()
        .filter_map(|(s, b)| (*s == 422).then_some(b))
        .collect();
    assert_eq!(
        accepted, 1,
        "exactly one concurrent submission must be accepted, outcomes: {outcomes:?}",
    );
    assert_eq!(
        rejected_bodies.len(),
        1,
        "exactly one concurrent submission must be rejected with 422, outcomes: {outcomes:?}",
    );
    let rejected_body = rejected_bodies[0];
    assert!(
        rejected_body.contains("bad nonce") || rejected_body.contains("INVALID_NONCE"),
        "rejected concurrent op should be nonce-class, got: {rejected_body}",
    );

    shutdown_runtime(runtime).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_replays_same_ordered_l2_tx_stream_from_db() {
    let db = temp_db("restart-replay-golden");
    let domain = test_domain();
    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    // Fund the sender via an ERC-20 deposit (becomes leading-range direct input).
    bootstrap_open_frame_with_deposits(db.path.as_str(), &[(sender, U256::from(1_000_000_u64))]);
    // Seed an additional safe direct input (arbitrary payload) for the restart-replay test.
    seed_safe_direct_input(db.path.as_str(), 10, vec![0xaa]);

    let Some(runtime) = start_full_server(db.path.as_str(), domain.clone()).await else {
        return;
    };

    let endpoint = format!("http://{}", runtime.addr);
    let client = SequencerClient::new_with_timeout(endpoint.clone(), Duration::from_secs(2))
        .expect("build sequencer client");
    let ws_url = client.ws_subscribe_url(0);
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), connect_async(ws_url))
        .await
        .expect("timeout connecting websocket")
        .expect("connect websocket");

    // First WS message: the deposit direct input from the leading range.
    let deposit_live = recv_ws_message(&mut ws).await;
    // Second WS message: the seeded safe direct input.
    let first_live = recv_ws_message(&mut ws).await;

    let request = make_valid_request(&domain);
    let (status, response_body) = client
        .submit_tx_with_status(&request)
        .await
        .expect("submit tx");
    assert_eq!(
        status, 200,
        "submit tx should succeed before restart: body={response_body}"
    );

    let second_live = recv_ws_message(&mut ws).await;
    drop(ws);

    let expected = all_ordered_l2_txs(db.path.as_str());
    assert_eq!(
        expected.len(),
        3,
        "expected deposit, direct input, and user op"
    );
    // DB offsets (SQLite rowid) start at 1.
    assert_ws_message_matches_tx(deposit_live, &expected[0], 1);
    assert_ws_message_matches_tx(first_live, &expected[1], 2);
    assert_ws_message_matches_tx(second_live, &expected[2], 3);

    shutdown_runtime(runtime).await;

    let Some(restarted) = start_full_server(db.path.as_str(), domain).await else {
        return;
    };

    let restarted_endpoint = format!("http://{}", restarted.addr);
    let restarted_client =
        SequencerClient::new_with_timeout(restarted_endpoint, Duration::from_secs(2))
            .expect("build sequencer client after restart");
    let restarted_ws_url = restarted_client.ws_subscribe_url(0);
    let (mut restarted_ws, _) =
        tokio::time::timeout(Duration::from_secs(5), connect_async(restarted_ws_url))
            .await
            .expect("timeout connecting websocket after restart")
            .expect("connect websocket after restart");

    for (i, expected_tx) in expected.iter().enumerate() {
        let replayed = recv_ws_message(&mut restarted_ws).await;
        // DB offsets start at 1.
        assert_ws_message_matches_tx(replayed, expected_tx, (i + 1) as u64);
    }
    drop(restarted_ws);

    shutdown_runtime(restarted).await;
}

struct FullServerRuntime {
    addr: std::net::SocketAddr,
    shutdown: ShutdownSignal,
    server_task: Option<http::ApiServerTask>,
    lane_handle: Option<
        tokio::task::JoinHandle<Result<(), sequencer::ingress::inclusion_lane::InclusionLaneError>>,
    >,
    _parked_rx: Option<mpsc::Receiver<PendingUserOp>>,
}

impl Drop for FullServerRuntime {
    fn drop(&mut self) {
        self.shutdown.request_shutdown();
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
        if let Some(task) = self.lane_handle.take() {
            task.abort();
        }
    }
}

async fn start_full_server(db_path: &str, domain: Eip712Domain) -> Option<FullServerRuntime> {
    start_full_server_with_max_body(db_path, domain, 128 * 1024).await
}

async fn start_full_server_with_max_body(
    db_path: &str,
    domain: Eip712Domain,
    max_body_bytes: usize,
) -> Option<FullServerRuntime> {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(value) => value,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "skipping e2e integration test: cannot bind test listener in this environment"
            );
            return None;
        }
        Err(err) => panic!("bind test listener: {err}"),
    };
    let addr = listener.local_addr().expect("read listener addr");

    let storage = Storage::open(db_path).expect("open storage");
    let shutdown = ShutdownSignal::default();

    let (tx, lane_handle) = InclusionLane::start(
        128,
        shutdown.clone(),
        WalletApp::new(WalletConfig::default()),
        storage,
        InclusionLaneConfig {
            batch_submitter_address: Address::from([0xff; 20]),
            max_user_ops_per_chunk: 32,
            safe_input_buffer_capacity: 32,
            max_batch_open: Duration::from_secs(60 * 60),
            idle_poll_interval: Duration::from_millis(2),
            frontier_min_interval: Duration::ZERO,
        },
    );

    let tx_feed = L2TxFeed::new(
        db_path.to_string(),
        shutdown.clone(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            batch_submitter_address: None,
        },
    );

    let server_task = http::start_on_listener(
        listener,
        tx,
        domain,
        MAX_METHOD_PAYLOAD_BYTES,
        shutdown.clone(),
        tx_feed,
        ApiConfig {
            max_body_bytes,
            ..ApiConfig::default()
        },
    );

    Some(FullServerRuntime {
        addr,
        shutdown,
        server_task: Some(server_task),
        lane_handle: Some(lane_handle),
        _parked_rx: None,
    })
}

async fn start_api_only_server(
    db_path: &str,
    domain: Eip712Domain,
    max_body_bytes: usize,
    queue_capacity: usize,
) -> Option<FullServerRuntime> {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(value) => value,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping api overload test: cannot bind test listener in this environment");
            return None;
        }
        Err(err) => panic!("bind test listener: {err}"),
    };
    let addr = listener.local_addr().expect("read listener addr");

    let _storage = Storage::open(db_path).expect("open storage");
    let (tx, rx) = mpsc::channel::<PendingUserOp>(queue_capacity);
    let shutdown = ShutdownSignal::default();
    let tx_feed = L2TxFeed::new(
        db_path.to_string(),
        shutdown.clone(),
        L2TxFeedConfig {
            idle_poll_interval: Duration::from_millis(2),
            page_size: 64,
            batch_submitter_address: None,
        },
    );
    let server_task = http::start_on_listener(
        listener,
        tx,
        domain,
        MAX_METHOD_PAYLOAD_BYTES,
        shutdown.clone(),
        tx_feed,
        ApiConfig {
            max_body_bytes,
            ..ApiConfig::default()
        },
    );

    Some(FullServerRuntime {
        addr,
        shutdown,
        server_task: Some(server_task),
        lane_handle: None,
        _parked_rx: Some(rx),
    })
}

async fn shutdown_runtime(mut runtime: FullServerRuntime) {
    runtime.shutdown.request_shutdown();
    if let Some(task) = runtime.lane_handle.take() {
        let lane_result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("wait for inclusion lane")
            .expect("join inclusion lane task");
        let ok =
            lane_result.is_ok() || matches!(lane_result, Err(InclusionLaneError::ChannelClosed));
        assert!(
            ok,
            "expected clean shutdown (Ok or ChannelClosed), got {lane_result:?}"
        );
    }
    if let Some(task) = runtime.server_task.take() {
        let server_result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("wait for server task")
            .expect("join server task");
        assert!(
            server_result.is_ok(),
            "expected clean server shutdown, got {server_result:?}"
        );
    }
}

fn bootstrap_open_frame(db_path: &str) {
    bootstrap_open_frame_with_deposits(db_path, &[]);
}

/// Bootstrap open frame, optionally seeding ERC-20 deposits for the given senders.
/// Each sender receives `amount` tokens before the frame is opened.
fn bootstrap_open_frame_with_deposits(db_path: &str, deposits: &[(Address, U256)]) {
    let mut storage = Storage::open(db_path).expect("open storage");
    let config = WalletConfig::default();

    // Always record a safe-head observation: production callers are gated by
    // `run_preemptive_recovery`, so storage paths like `safe_input_frontier`
    // assume a row exists. With no deposits we still write an empty advance
    // so the lane can start without `current_safe_block_required` failing.
    let safe_inputs: Vec<StoredSafeInput> = deposits
        .iter()
        .map(|(sender, amount)| {
            let mut payload = Vec::with_capacity(72);
            payload.extend_from_slice(config.supported_erc20_token.as_slice());
            payload.extend_from_slice(sender.as_slice());
            payload.extend_from_slice(amount.to_be_bytes::<32>().as_slice());
            StoredSafeInput {
                sender: config.erc20_portal_address,
                payload,
                block_number: 1,
            }
        })
        .collect();
    storage
        .append_safe_inputs(
            1,
            &safe_inputs,
            Address::ZERO,
            &sequencer_core::protocol::ProtocolTiming {
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
        )
        .expect("seed safe head (and any deposits)");

    let safe_input_count = deposits.len() as u64;
    let leading_range = SafeInputRange::new(0, safe_input_count);
    // Default log_gas_price=0 → log_recommended_fee = 0+20+419+621 = 1060.
    let head = storage
        .initialize_open_state(1, leading_range)
        .expect("initialize open state");
    assert_eq!(head.frame_fee, 1060);
}

/// Default max_fee for test fixtures: must be >= default log_recommended_fee (1060).
const TEST_MAX_FEE: u16 = 1200;

fn make_valid_request(domain: &Eip712Domain) -> TxRequest {
    let signing_key = SigningKey::from_bytes((&[7_u8; 32]).into()).expect("create signing key");
    let sender = address_from_signing_key(&signing_key);
    let method = Method::Withdrawal(Withdrawal {
        amount: U256::from(0_u64),
    });
    let user_op = UserOp {
        nonce: 0,
        max_fee: TEST_MAX_FEE,
        data: ssz::Encode::as_ssz_bytes(&method).into(),
    };
    let signature_hex = sign_user_op_hex(domain, &user_op, &signing_key);
    TxRequest {
        message: user_op,
        signature: signature_hex,
        sender: sender.to_string(),
    }
}

fn seed_safe_direct_input(db_path: &str, safe_block: u64, payload: Vec<u8>) {
    let mut storage = Storage::open(db_path).expect("open storage");
    storage
        .append_safe_inputs(
            safe_block,
            &[StoredSafeInput {
                sender: Address::ZERO,
                payload,
                block_number: safe_block,
            }],
            Address::ZERO,
            &sequencer_core::protocol::ProtocolTiming {
                max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
                preemptive_margin_blocks: 75,
                l1_read_stale_after_blocks: 900,
                seconds_per_block: 12,
            },
        )
        .expect("append safe direct input");
}

fn all_ordered_l2_txs(db_path: &str) -> Vec<SequencedL2Tx> {
    let mut storage = Storage::open_read_only(db_path).expect("open read-only storage");
    storage
        .ordered_l2_txs_page_from(0, 1_000_000)
        .expect("load ordered l2 txs")
        .into_iter()
        .map(|(_offset, tx)| tx)
        .collect()
}

fn assert_ws_message_matches_tx(
    actual: WsTxMessage,
    expected: &SequencedL2Tx,
    expected_offset: u64,
) {
    match (actual, expected) {
        (
            WsTxMessage::UserOp {
                offset,
                sender,
                fee,
                data,
            },
            SequencedL2Tx::UserOp(expected),
        ) => {
            assert_eq!(offset, expected_offset);
            assert_eq!(
                decode_hex_prefixed(sender.as_str()),
                expected.sender.as_slice()
            );
            assert_eq!(fee, expected.fee);
            assert_eq!(decode_hex_prefixed(data.as_str()), expected.data.as_slice());
        }
        (
            WsTxMessage::DirectInput {
                offset,
                sender,
                block_number,
                payload,
            },
            SequencedL2Tx::Direct(expected),
        ) => {
            assert_eq!(offset, expected_offset);
            assert_eq!(
                decode_hex_prefixed(sender.as_str()),
                expected.sender.as_slice()
            );
            assert_eq!(block_number, expected.block_number);
            assert_eq!(
                decode_hex_prefixed(payload.as_str()),
                expected.payload.as_slice()
            );
        }
        (actual, expected) => {
            panic!(
                "expected websocket message to match persisted tx, got {actual:?} vs {expected:?}"
            );
        }
    }
}

async fn post_raw_body_no_content_type(addr: std::net::SocketAddr, body: &str) -> (u16, String) {
    let host_port = addr.to_string();
    let mut stream = tokio::net::TcpStream::connect(host_port.as_str())
        .await
        .expect("connect test http socket");
    // Deliberately omit Content-Type header.
    let request = format!(
        "POST /tx HTTP/1.1\r\nHost: {host_port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write raw request");
    stream.flush().await.expect("flush raw request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read raw response");
    parse_http_response(response.as_slice())
}

async fn post_raw_json(addr: std::net::SocketAddr, body: &str) -> (u16, String) {
    let host_port = addr.to_string();
    let mut stream = tokio::net::TcpStream::connect(host_port.as_str())
        .await
        .expect("connect test http socket");
    let request = format!(
        "POST /tx HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write raw request");
    stream.flush().await.expect("flush raw request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read raw response");
    parse_http_response(response.as_slice())
}

fn parse_http_response(raw: &[u8]) -> (u16, String) {
    let text = String::from_utf8(raw.to_vec()).expect("response is valid utf8");
    let mut sections = text.splitn(2, "\r\n\r\n");
    let headers = sections.next().unwrap_or_default();
    let body = sections.next().unwrap_or_default().to_string();
    let status = headers
        .lines()
        .next()
        .expect("status line exists")
        .split_whitespace()
        .nth(1)
        .expect("status code exists")
        .parse::<u16>()
        .expect("status code parses");
    (status, body)
}

fn sign_user_op_hex(domain: &Eip712Domain, user_op: &UserOp, signing_key: &SigningKey) -> String {
    let hash = user_op.eip712_signing_hash(domain);
    let k256_sig = signing_key
        .sign_prehash(hash.as_slice())
        .expect("sign user op hash");

    let sender = address_from_signing_key(signing_key);
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|candidate| {
            candidate
                .recover_address_from_prehash(&hash)
                .ok()
                .map(|value| value == sender)
                .unwrap_or(false)
        })
        .expect("recoverable parity for signature");

    alloy_primitives::hex::encode_prefixed(signature.as_bytes())
}

fn address_from_signing_key(signing_key: &SigningKey) -> Address {
    let verifying = signing_key.verifying_key().to_encoded_point(false);
    Address::from_raw_public_key(&verifying.as_bytes()[1..])
}

async fn recv_ws_message(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> WsTxMessage {
    let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("wait for websocket frame")
        .expect("websocket stream ended")
        .expect("receive websocket frame");
    match frame {
        Message::Text(value) => serde_json::from_str(value.as_str()).expect("parse ws payload"),
        other => panic!("expected text ws frame, got {other:?}"),
    }
}

fn decode_hex_prefixed(value: &str) -> Vec<u8> {
    assert!(value.starts_with("0x"), "hex field must be 0x-prefixed");
    alloy_primitives::hex::decode(value).expect("decode hex")
}

fn test_domain() -> Eip712Domain {
    sequencer_core::build_input_domain(1, Address::from_slice(&[0_u8; 20]))
}
