#!/usr/bin/env python3
"""
Generate N random wallets and write them in the benchmark accounts-file format.

Output format (one per line):
    <0x-address> <0x-private-key>

Requires: `cast` (Foundry) on PATH.

Usage:
    python3 gen_accounts.py --count 16 --output sepolia_accounts.txt
"""

import argparse
import subprocess
import sys


def generate_wallet() -> tuple:
    """Generate a random wallet using `cast wallet new`, return (address, private_key)."""
    out = subprocess.check_output(["cast", "wallet", "new"], text=True)
    address = None
    private_key = None
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("Address:"):
            address = line.split(":", 1)[1].strip()
        elif line.startswith("Private key:"):
            private_key = line.split(":", 1)[1].strip()
    if not address or not private_key:
        print(f"failed to parse cast wallet new output:\n{out}", file=sys.stderr)
        sys.exit(1)
    return address, private_key


def main():
    parser = argparse.ArgumentParser(description="Generate random wallets for benchmarking")
    parser.add_argument("--count", type=int, default=16, help="number of wallets to generate")
    parser.add_argument("--output", type=str, default="sepolia_accounts.txt",
                        help="output file path")
    args = parser.parse_args()

    print(f"generating {args.count} wallets...")
    lines = []
    for i in range(args.count):
        address, private_key = generate_wallet()
        lines.append(f"{address} {private_key}")
        if (i + 1) % 10 == 0:
            print(f"  {i + 1}/{args.count}")

    with open(args.output, "w") as f:
        f.write("\n".join(lines) + "\n")

    print(f"wrote {args.count} accounts to {args.output}")


if __name__ == "__main__":
    main()
