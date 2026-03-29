#!/usr/bin/env python3
"""
Quick demo script: submit self-transfer user ops to a live Sepolia sequencer,
printing ACK latency for each transaction.

Requires: Python 3.8+, `cast` (Foundry) on PATH.

Usage:
    export SEQUENCER_URL=https://eth-sepolia.rollups.cartesi.io/v2
    export PRIVATE_KEY=0x...
    export APP_ADDRESS=0x...
    export SEPOLIA_CHAIN_ID=11155111

    python3 demo_sepolia.py --start-nonce 0 --count 20
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request

# ── config from env ──────────────────────────────────────────────────────────

SEQUENCER_URL = os.environ.get("SEQUENCER_URL", "https://eth-sepolia.rollups.cartesi.io/v2")
PRIVATE_KEY = os.environ["PRIVATE_KEY"]
APP_ADDRESS = os.environ["APP_ADDRESS"]
CHAIN_ID = int(os.environ.get("SEPOLIA_CHAIN_ID", "11155111"))

# derive sender address once
SENDER = subprocess.check_output(
    ["cast", "wallet", "address", "--private-key", PRIVATE_KEY], text=True
).strip()

# ── EIP-712 typed data (matches deployed Sepolia sequencer: max_fee is uint32) ─

TYPED_DATA_TEMPLATE = {
    "types": {
        "EIP712Domain": [
            {"name": "name", "type": "string"},
            {"name": "version", "type": "string"},
            {"name": "chainId", "type": "uint256"},
            {"name": "verifyingContract", "type": "address"},
        ],
        "UserOp": [
            {"name": "nonce", "type": "uint32"},
            {"name": "max_fee", "type": "uint32"},
            {"name": "data", "type": "bytes"},
        ],
    },
    "primaryType": "UserOp",
    "domain": {
        "name": "CartesiAppSequencer",
        "version": "1",
        "chainId": CHAIN_ID,
        "verifyingContract": APP_ADDRESS,
    },
}


def build_transfer_payload(amount: int, recipient: str) -> str:
    """Encode a Transfer method call: 0x01 | amount (32 LE) | to (20 bytes)."""
    return "0x01" + amount.to_bytes(32, "little").hex() + recipient[2:].lower()


def sign_and_build_request(nonce: int, max_fee: int, data_hex: str) -> str:
    typed_data = dict(TYPED_DATA_TEMPLATE)
    typed_data["message"] = {"nonce": nonce, "max_fee": max_fee, "data": data_hex}
    td_json = json.dumps(typed_data, separators=(",", ":"))

    sig = subprocess.check_output(
        ["cast", "wallet", "sign", "--data", td_json, "--private-key", PRIVATE_KEY],
        text=True,
    ).strip()

    return json.dumps(
        {
            "message": {"nonce": nonce, "max_fee": max_fee, "data": data_hex},
            "signature": sig,
            "sender": SENDER,
        },
        separators=(",", ":"),
    )


def submit(body: str) -> tuple:
    """POST to /tx, return (status, body, elapsed_ms)."""
    url = f"{SEQUENCER_URL.rstrip('/')}/tx"
    req = urllib.request.Request(
        url, data=body.encode(), headers={"Content-Type": "application/json"}
    )
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            elapsed = (time.perf_counter() - t0) * 1000
            return resp.status, resp.read().decode(), elapsed
    except urllib.error.HTTPError as e:
        elapsed = (time.perf_counter() - t0) * 1000
        return e.code, e.read().decode(), elapsed


def ping(rounds: int = 5) -> list:
    """Measure infra RTT with invalid POSTs (rejected before sequencer logic)."""
    url = f"{SEQUENCER_URL.rstrip('/')}/tx"
    body = b'{"invalid":true}'
    samples = []
    for _ in range(rounds):
        req = urllib.request.Request(
            url, data=body, headers={"Content-Type": "application/json"}
        )
        t0 = time.perf_counter()
        try:
            urllib.request.urlopen(req, timeout=10)
        except urllib.error.HTTPError:
            pass
        samples.append((time.perf_counter() - t0) * 1000)
    return samples


def discover_nonce() -> int:
    """Send nonce=0; the 422 response leaks the expected nonce."""
    body = sign_and_build_request(0, 0, build_transfer_payload(1, SENDER))
    status, resp_body, _ = submit(body)
    if status == 200:
        return 1  # nonce 0 was accepted, next is 1
    try:
        msg = json.loads(resp_body).get("message", "")
        # "bad nonce: expected 1083, got 0"
        if "expected" in msg:
            return int(msg.split("expected")[1].split(",")[0].strip())
    except Exception:
        pass
    print(f"could not discover nonce (status={status}): {resp_body}", file=sys.stderr)
    sys.exit(1)


# ── pretty output ────────────────────────────────────────────────────────────

BOLD = "\033[1m"
DIM = "\033[2m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
RED = "\033[31m"
RESET = "\033[0m"


def main():
    parser = argparse.ArgumentParser(description="Sepolia sequencer demo")
    parser.add_argument("--start-nonce", type=int, default=None,
                        help="omit to auto-discover from the sequencer")
    parser.add_argument("--count", type=int, default=20)
    parser.add_argument("--amount", type=int, default=1, help="transfer amount")
    parser.add_argument("--max-fee", type=int, default=0)
    parser.add_argument("--delay", type=float, default=0.0, help="seconds between txs")
    parser.add_argument("--ping-rounds", type=int, default=5, help="number of ping probes")
    args = parser.parse_args()

    print(f"{DIM}pinging sequencer ({args.ping_rounds}x)...{RESET}", end=" ", flush=True)
    ping_samples = ping(args.ping_rounds)
    avg_rtt = sum(ping_samples) / len(ping_samples)
    each = " ".join(f"{s:.0f}" for s in ping_samples)
    print(f"avg {avg_rtt:.0f}ms  [{each}]")

    start_nonce = args.start_nonce
    if start_nonce is None:
        print(f"{DIM}discovering current nonce...{RESET}", end=" ", flush=True)
        start_nonce = discover_nonce()
        print(f"{start_nonce}")

    payload = build_transfer_payload(args.amount, SENDER)

    print()
    print(f"{BOLD}Sepolia sequencer demo{RESET}")
    print(f"  endpoint:  {SEQUENCER_URL}")
    print(f"  infra RTT: ~{avg_rtt:.0f}ms  {DIM}(network + proxy, no sequencer processing){RESET}")
    print(f"  sender:    {SENDER}")
    print(f"  tx type:   self-transfer ({args.amount} unit{'s' if args.amount != 1 else ''})")
    print(f"  nonces:    {start_nonce}..{start_nonce + args.count - 1}")
    print()
    print(f"{'nonce':>7}  {'status':>6}  {'ack':>9}  {'avg':>9}  description")
    print(f"{'─'*7}  {'─'*6}  {'─'*9}  {'─'*9}  {'─'*30}")

    latencies = []
    ok = 0

    for i in range(args.count):
        nonce = start_nonce + i
        body = sign_and_build_request(nonce, args.max_fee, payload)
        status, resp_body, elapsed = submit(body)

        latencies.append(elapsed)
        avg = sum(latencies) / len(latencies)

        if status == 200:
            ok += 1
            color = GREEN
            tag = "OK"
            desc = f"transfer {args.amount} to self, nonce {nonce}"
        else:
            color = RED if status >= 500 else YELLOW
            try:
                msg = json.loads(resp_body).get("message", resp_body[:40])
            except Exception:
                msg = resp_body[:40]
            tag = str(status)
            desc = msg

        print(
            f"  {nonce:>5}  {color}{tag:>6}{RESET}"
            f"  {elapsed:>7.1f}ms  {avg:>7.1f}ms"
            f"  {DIM}{desc}{RESET}"
        )

        if args.delay > 0:
            time.sleep(args.delay)

    print()
    print(f"{BOLD}done:{RESET} {ok}/{args.count} accepted, avg ack {sum(latencies)/len(latencies):.1f}ms")


if __name__ == "__main__":
    main()
