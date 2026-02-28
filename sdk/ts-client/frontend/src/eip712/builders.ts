import { encodePacked, hashTypedData, keccak256, type Address, type Hex } from "viem";

import type { SnapshotResponse } from "../api/types";
import {
  EIP712_DOMAIN_NAME,
  EIP712_DOMAIN_VERSION,
  SESSION_KEY_GRANT_TYPES,
  TRADING_ENVELOPE_TYPES,
  type SessionKeyGrantMessage,
  type TradingEnvelopeMessage,
} from "./types";

export function parseBigIntInput(raw: string, label: string): bigint {
  const trimmed = raw.trim();
  if (trimmed.length === 0) {
    throw new Error(`${label} is required`);
  }
  try {
    return BigInt(trimmed);
  } catch {
    throw new Error(`${label} must be a valid integer`);
  }
}

export function createDomain(snapshot: SnapshotResponse) {
  return {
    name: EIP712_DOMAIN_NAME,
    version: EIP712_DOMAIN_VERSION,
    chainId: snapshot.chainId,
    verifyingContract: snapshot.app,
  } as const;
}

export function buildGrantTypedData(snapshot: SnapshotResponse, message: SessionKeyGrantMessage) {
  return {
    domain: createDomain(snapshot),
    types: SESSION_KEY_GRANT_TYPES,
    primaryType: "SessionKeyGrant" as const,
    message,
  };
}

export function buildTradingEnvelopeTypedData(snapshot: SnapshotResponse, message: TradingEnvelopeMessage) {
  return {
    domain: createDomain(snapshot),
    types: TRADING_ENVELOPE_TYPES,
    primaryType: "TradingEnvelope" as const,
    message,
  };
}

export function hashGrantId(snapshot: SnapshotResponse, message: SessionKeyGrantMessage): Hex {
  const typed = buildGrantTypedData(snapshot, message);
  return hashTypedData(typed);
}

export function computeActionHash(actionBytes: Hex): Hex {
  return keccak256(actionBytes);
}

export function buildPlaceOrderActionBytes(args: {
  symbol: bigint;
  price: bigint;
  priceExp: bigint;
  size: bigint;
  sizeExp: bigint;
  side: bigint;
  tif: bigint;
  postOnly: bigint;
  reduceOnly: bigint;
}): Hex {
  return encodePacked(
    ["uint8", "uint64", "uint64", "uint8", "uint64", "uint8", "uint8", "uint8", "uint8", "uint8"],
    [
      3,
      args.symbol,
      args.price,
      args.priceExp,
      args.size,
      args.sizeExp,
      args.side,
      args.tif,
      args.postOnly,
      args.reduceOnly,
    ],
  );
}

export function buildCancelActionBytes(args: { orderId: bigint }): Hex {
  return encodePacked(["uint8", "uint64"], [4, args.orderId]);
}

export function buildWithdrawActionBytes(args: { amount: bigint; amountExp: bigint }): Hex {
  return encodePacked(["uint8", "uint64", "uint8"], [2, args.amount, args.amountExp]);
}

export function asAddress(raw: string, label: string): Address {
  if (!/^0x[a-fA-F0-9]{40}$/.test(raw)) {
    throw new Error(`${label} must be a valid 20-byte hex address`);
  }
  return raw as Address;
}
