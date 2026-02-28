import {
  hashTypedData,
  hexToBytes,
  keccak256,
  recoverTypedDataAddress,
  type Hex,
} from "viem";

import { ACTION_CANCEL, ACTION_PLACE, ACTION_WITHDRAW } from "./actions.js";
import type { SequencerConfig } from "./config.js";
import { HttpError } from "./errors.js";
import { normalizeHex, parseAddress, parseBigIntValue } from "./utils.js";

export const EIP712_DOMAIN_NAME = "CartesiDex";
export const EIP712_DOMAIN_VERSION = "1";

export const SESSION_SCOPE_TRADE = 1n;
export const SESSION_SCOPE_CANCEL = 2n;
export const SESSION_SCOPE_WITHDRAW = 4n;

export const ZERO_GRANT_ID = `0x${"0".repeat(64)}` as Hex;

export const SESSION_KEY_GRANT_PRIMARY_TYPE = "SessionKeyGrant";
export const SESSION_KEY_GRANT_TYPES = {
  SessionKeyGrant: [
    { name: "owner", type: "address" },
    { name: "sessionKey", type: "address" },
    { name: "validUntilTickId", type: "uint256" },
    { name: "scopes", type: "uint256" },
  ],
} as const;

export const TRADING_ENVELOPE_PRIMARY_TYPE = "TradingEnvelope";
export const TRADING_ENVELOPE_TYPES = {
  TradingEnvelope: [
    { name: "owner", type: "address" },
    { name: "nonce", type: "uint256" },
    { name: "minInputIndexPlusOne", type: "uint256" },
    { name: "validUntilTickId", type: "uint256" },
    { name: "actionHash", type: "bytes32" },
    { name: "grantId", type: "bytes32" },
  ],
} as const;

export type SessionKeyGrantMessage = {
  owner: string;
  sessionKey: string;
  validUntilTickId: string;
  scopes: string;
};

export type NormalizedSessionKeyGrantMessage = {
  owner: `0x${string}`;
  sessionKey: `0x${string}`;
  validUntilTickId: string;
  scopes: string;
};

export type TradingEnvelopeMessage = {
  owner: string;
  nonce: string;
  minInputIndexPlusOne: string;
  validUntilTickId: string;
  actionBytes: Hex;
  grantId: string;
};

export type NormalizedTradingEnvelopeMessage = {
  owner: `0x${string}`;
  nonce: string;
  minInputIndexPlusOne: string;
  validUntilTickId: string;
  actionBytes: Hex;
  grantId: Hex;
};

export function buildSequencerDomain(config: SequencerConfig) {
  return {
    name: EIP712_DOMAIN_NAME,
    version: EIP712_DOMAIN_VERSION,
    chainId: config.chainId,
    verifyingContract: config.appAddress,
  } as const;
}

export function normalizeSessionKeyGrantMessage(
  message: SessionKeyGrantMessage,
): NormalizedSessionKeyGrantMessage {
  const owner = parseAddress(message.owner, "grant.owner");
  const sessionKey = parseAddress(message.sessionKey, "grant.sessionKey");
  const validUntilTickId = parseNonNegativeBigInt(message.validUntilTickId, "grant.validUntilTickId");
  const scopes = parseNonNegativeBigInt(message.scopes, "grant.scopes");
  return {
    owner,
    sessionKey,
    validUntilTickId: validUntilTickId.toString(),
    scopes: scopes.toString(),
  };
}

export function hashSessionKeyGrant(
  config: SequencerConfig,
  message: NormalizedSessionKeyGrantMessage,
): Hex {
  return hashTypedData({
    domain: buildSequencerDomain(config),
    types: SESSION_KEY_GRANT_TYPES,
    primaryType: SESSION_KEY_GRANT_PRIMARY_TYPE,
    message: {
      owner: message.owner,
      sessionKey: message.sessionKey,
      validUntilTickId: BigInt(message.validUntilTickId),
      scopes: BigInt(message.scopes),
    },
  });
}

export async function recoverSessionKeyGrantSigner(args: {
  config: SequencerConfig;
  message: NormalizedSessionKeyGrantMessage;
  signature: string;
}): Promise<`0x${string}`> {
  const signature = normalizeSignature(args.signature, "ownerSig");
  return recoverTypedDataAddress({
    domain: buildSequencerDomain(args.config),
    types: SESSION_KEY_GRANT_TYPES,
    primaryType: SESSION_KEY_GRANT_PRIMARY_TYPE,
    message: {
      owner: args.message.owner,
      sessionKey: args.message.sessionKey,
      validUntilTickId: BigInt(args.message.validUntilTickId),
      scopes: BigInt(args.message.scopes),
    },
    signature,
  });
}

export function normalizeTradingEnvelopeMessage(
  message: TradingEnvelopeMessage,
): NormalizedTradingEnvelopeMessage {
  const owner = parseAddress(message.owner, "envelope.owner");
  const nonce = parseNonNegativeBigInt(message.nonce, "envelope.nonce");
  const minInputIndexPlusOne = parseNonNegativeBigInt(
    message.minInputIndexPlusOne,
    "envelope.minInputIndexPlusOne",
  );
  const validUntilTickId = parseNonNegativeBigInt(message.validUntilTickId, "envelope.validUntilTickId");
  const actionBytes = normalizeHex(message.actionBytes, "envelope.actionBytes");
  const grantId = normalizeHex(message.grantId, "envelope.grantId");
  if (hexToBytes(grantId).length !== 32) {
    throw new HttpError(400, "envelope.grantId must be 32 bytes");
  }
  return {
    owner,
    nonce: nonce.toString(),
    minInputIndexPlusOne: minInputIndexPlusOne.toString(),
    validUntilTickId: validUntilTickId.toString(),
    actionBytes,
    grantId,
  };
}

export async function recoverTradingEnvelopeSigner(args: {
  config: SequencerConfig;
  message: NormalizedTradingEnvelopeMessage;
  signature: string;
}): Promise<`0x${string}`> {
  const signature = normalizeSignature(args.signature, "signature");
  return recoverTypedDataAddress({
    domain: buildSequencerDomain(args.config),
    types: TRADING_ENVELOPE_TYPES,
    primaryType: TRADING_ENVELOPE_PRIMARY_TYPE,
    message: {
      owner: args.message.owner,
      nonce: BigInt(args.message.nonce),
      minInputIndexPlusOne: BigInt(args.message.minInputIndexPlusOne),
      validUntilTickId: BigInt(args.message.validUntilTickId),
      actionHash: keccak256(args.message.actionBytes),
      grantId: args.message.grantId,
    },
    signature,
  });
}

export function requiredScopeForActionType(actionType: number): bigint | null {
  if (actionType === ACTION_PLACE) {
    return SESSION_SCOPE_TRADE;
  }
  if (actionType === ACTION_CANCEL) {
    return SESSION_SCOPE_CANCEL;
  }
  if (actionType === ACTION_WITHDRAW) {
    return SESSION_SCOPE_WITHDRAW;
  }
  return null;
}

export function hasScope(scopes: bigint, required: bigint): boolean {
  return (scopes & required) === required;
}

export function isZeroGrantId(grantId: Hex): boolean {
  return grantId.toLowerCase() === ZERO_GRANT_ID;
}

export function normalizeSignature(value: string, field: string): Hex {
  const normalized = normalizeHex(value, field);
  if (hexToBytes(normalized).length !== 65) {
    throw new HttpError(400, `${field} must be 65 bytes`);
  }
  return normalized;
}

function parseNonNegativeBigInt(raw: string, field: string): bigint {
  const parsed = parseBigIntValue(raw, field);
  if (parsed < 0n) {
    throw new HttpError(400, `${field} must be non-negative`);
  }
  return parsed;
}
