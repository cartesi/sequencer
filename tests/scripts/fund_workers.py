#!/usr/bin/env python3
"""
Fan-out L2 balance from the funded wallet to worker accounts.

Reads worker addresses from an accounts file (gen_accounts.py output format)
and submits Transfer user-ops from the funded wallet to each worker.

Requires: Python 3.8+, `cast` (Foundry) on PATH.

Usage:
    python3 fund_workers.py --accounts-file sepolia_accounts.txt --amount-per-worker 10000
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
MAX_FEE = int(os.environ.get("MAX_FEE", "1200"))

SENDER = subprocess.check_output(
    ["cast", "wallet", "address", "--private-key", PRIVATE_KEY], text=True
).strip()

# ── EIP-712 typed data ──────────────────────────────────────────────────────

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
            {"name": "max_fee", "type": "uint16"},
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


def discover_nonce() -> int:
    """Send nonce=0; the 422 response leaks the expected nonce."""
    body = sign_and_build_request(0, MAX_FEE, build_transfer_payload(1, SENDER))
    status, resp_body, _ = submit(body)
    if status == 200:
        return 1
    try:
        msg = json.loads(resp_body).get("message", "")
        if "expected" in msg:
            return int(msg.split("expected")[1].split(",")[0].strip())
    except Exception:
        pass
    print(f"could not discover nonce (status={status}): {resp_body}", file=sys.stderr)
    sys.exit(1)


def load_accounts_file(path: str) -> list:
    """Load accounts file, return list of (address, private_key)."""
    accounts = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            parts = line.split()
            if len(parts) != 2:
                print(f"bad line in accounts file: {line}", file=sys.stderr)
                sys.exit(1)
            accounts.append((parts[0], parts[1]))
    return accounts


BOLD = "\033[1m"
DIM = "\033[2m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
RED = "\033[31m"
RESET = "\033[0m"


def main():
    parser = argparse.ArgumentParser(description="Fund worker accounts via L2 transfers")
    parser.add_argument("--accounts-file", type=str, required=True,
                        help="path to accounts file (gen_accounts.py output)")
    parser.add_argument("--amount-per-worker", type=int, required=True,
                        help="amount to transfer to each worker")
    parser.add_argument("--count", type=int, default=None,
                        help="fund only the first N accounts (default: all)")
    parser.add_argument("--start-nonce", type=int, default=None,
                        help="omit to auto-discover")
    parser.add_argument("--max-fee", type=int, default=None,
                        help="override MAX_FEE from env")
    args = parser.parse_args()

    max_fee = args.max_fee if args.max_fee is not None else MAX_FEE
    accounts = load_accounts_file(args.accounts_file)
    if not accounts:
        print("no accounts found in file", file=sys.stderr)
        sys.exit(1)
    if args.count is not None:
        accounts = accounts[:args.count]

    start_nonce = args.start_nonce
    if start_nonce is None:
        print(f"{DIM}discovering current nonce...{RESET}", end=" ", flush=True)
        start_nonce = discover_nonce()
        print(f"{start_nonce}")

    print()
    print(f"{BOLD}funding {len(accounts)} worker accounts{RESET}")
    print(f"  endpoint:          {SEQUENCER_URL}")
    print(f"  sender:            {SENDER}")
    print(f"  amount per worker: {args.amount_per_worker}")
    print(f"  max_fee:           {max_fee}")
    print(f"  start nonce:       {start_nonce}")
    print()
    print(f"{'#':>5}  {'status':>6}  {'ack':>9}  address")
    print(f"{'─'*5}  {'─'*6}  {'─'*9}  {'─'*44}")

    ok = 0
    for i, (address, _private_key) in enumerate(accounts):
        nonce = start_nonce + i
        payload = build_transfer_payload(args.amount_per_worker, address)
        body = sign_and_build_request(nonce, max_fee, payload)
        status, resp_body, elapsed = submit(body)

        if status == 200:
            ok += 1
            color = GREEN
            tag = "OK"
        else:
            color = RED if status >= 500 else YELLOW
            tag = str(status)

        print(f"  {i+1:>3}  {color}{tag:>6}{RESET}  {elapsed:>7.1f}ms  {address}")

        if status != 200:
            try:
                msg = json.loads(resp_body).get("message", resp_body[:60])
            except Exception:
                msg = resp_body[:60]
            print(f"       {DIM}{msg}{RESET}")

    print()
    print(f"{BOLD}done:{RESET} {ok}/{len(accounts)} funded")


if __name__ == "__main__":
    main()
