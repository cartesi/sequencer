#!/usr/bin/env bash
set -euo pipefail

# Deposit USDC into the Cartesi app via the ERC20Portal on Sepolia.
# Reads configuration from .env (loaded by justfile or sourced manually).

: "${L1_RPC_URL:?L1_RPC_URL not set}"
: "${PRIVATE_KEY:?PRIVATE_KEY not set}"
: "${APP_ADDRESS:?APP_ADDRESS not set}"
: "${ERC20_PORTAL:?ERC20_PORTAL not set}"
: "${USDC:?USDC not set}"
: "${DEPOSIT_AMOUNT:?DEPOSIT_AMOUNT not set}"

echo "=== Sepolia USDC Deposit ==="
echo "  token:      $USDC"
echo "  portal:     $ERC20_PORTAL"
echo "  app:        $APP_ADDRESS"
echo "  amount:     $DEPOSIT_AMOUNT"
echo

echo "step 1/2: approving ERC20Portal to spend $DEPOSIT_AMOUNT USDC..."
cast send "$USDC" \
    "approve(address,uint256)" \
    "$ERC20_PORTAL" "$DEPOSIT_AMOUNT" \
    --rpc-url "$L1_RPC_URL" \
    --private-key "$PRIVATE_KEY"

echo
echo "step 2/2: depositing $DEPOSIT_AMOUNT USDC via ERC20Portal..."
cast send "$ERC20_PORTAL" \
    "depositERC20Tokens(address,address,uint256,bytes)" \
    "$USDC" "$APP_ADDRESS" "$DEPOSIT_AMOUNT" "0x" \
    --rpc-url "$L1_RPC_URL" \
    --private-key "$PRIVATE_KEY"

echo
echo "deposit submitted. It will be credited after Safe finality (~6-7 min on Sepolia)."
