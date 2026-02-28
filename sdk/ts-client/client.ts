import { encodePacked, hashTypedData, keccak256, type Hex } from "viem";
import { z } from "zod";

import {
  EIP712_DOMAIN_NAME,
  EIP712_DOMAIN_VERSION,
  SESSION_KEY_GRANT_PRIMARY_TYPE,
  SESSION_KEY_GRANT_TYPES,
  TRADING_ENVELOPE_PRIMARY_TYPE,
  TRADING_ENVELOPE_TYPES,
  ZERO_GRANT_ID,
  type SessionKeyGrantMessage,
  type TradingEnvelopeMessage,
} from "./eip712.js";

const snapshotSchema = z.object({
  chainId: z.number().int().nonnegative(),
  app: z.string().regex(/^0x[a-fA-F0-9]{40}$/),
  minInputIndexPlusOne: z.string(),
  windowId: z.string(),
  windowEndTs: z.string(),
});

const registerGrantResponseSchema = z.object({
  ok: z.boolean(),
  grantId: z.string().regex(/^0x[a-fA-F0-9]{64}$/),
});

const submitReceiptSchema = z.object({
  accepted: z.boolean(),
  windowId: z.string(),
  seq: z.number().int().nonnegative(),
  minInputIndexPlusOne: z.string(),
  envelopeHash: z.string().regex(/^0x[a-fA-F0-9]{64}$/),
  actionBytes: z.string().regex(/^0x[a-fA-F0-9]+$/),
  authMode: z.enum(["OWNER_SIG", "SESSION_SIG"]),
  signerRecovered: z.string().regex(/^0x[a-fA-F0-9]{40}$/),
  grantId: z
    .string()
    .regex(/^0x[a-fA-F0-9]{64}$/)
    .optional(),
});

export type SnapshotResponse = z.infer<typeof snapshotSchema>;
export type RegisterGrantResponse = z.infer<typeof registerGrantResponseSchema>;
export type SubmitEnvelopeReceipt = z.infer<typeof submitReceiptSchema>;

export class SequencerClientError extends Error {
  public readonly statusCode: number;
  public readonly details: unknown;

  public constructor(statusCode: number, message: string, details: unknown) {
    super(message);
    this.statusCode = statusCode;
    this.details = details;
  }
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;
type IntegerInput = bigint | number | string;
type FlagInput = boolean | IntegerInput;

const UINT8_MAX = 255n;
const UINT64_MAX = (1n << 64n) - 1n;

export type SequencerDomain = {
  name: string;
  version: string;
  chainId: number;
  verifyingContract: `0x${string}`;
};

export type SessionKeyGrantTypedData = {
  domain: SequencerDomain;
  types: typeof SESSION_KEY_GRANT_TYPES;
  primaryType: typeof SESSION_KEY_GRANT_PRIMARY_TYPE;
  message: {
    owner: `0x${string}`;
    sessionKey: `0x${string}`;
    validUntilTickId: bigint;
    scopes: bigint;
  };
};

export type TradingEnvelopeTypedData = {
  domain: SequencerDomain;
  types: typeof TRADING_ENVELOPE_TYPES;
  primaryType: typeof TRADING_ENVELOPE_PRIMARY_TYPE;
  message: {
    owner: `0x${string}`;
    nonce: bigint;
    minInputIndexPlusOne: bigint;
    validUntilTickId: bigint;
    actionHash: Hex;
    grantId: Hex;
  };
};

export type WithdrawCollateralActionParams = {
  amount: IntegerInput;
  amountExponent?: IntegerInput;
};

export type PlaceOrderActionParams = {
  symbol: IntegerInput;
  price: IntegerInput;
  priceExponent?: IntegerInput;
  size: IntegerInput;
  sizeExponent?: IntegerInput;
  side: IntegerInput;
  timeInForce: IntegerInput;
  postOnly?: FlagInput;
  reduceOnly?: FlagInput;
};

export type CancelOrderActionParams = {
  orderId: IntegerInput;
};

export async function getSnapshot(
  baseUrl: string,
  fetchImpl: FetchLike = fetch,
): Promise<SnapshotResponse> {
  const json = await requestJson(baseUrl, "/v0/snapshot", { method: "GET" }, fetchImpl);
  return snapshotSchema.parse(json);
}

export function createSequencerDomain(snapshot: SnapshotResponse): SequencerDomain {
  return {
    name: EIP712_DOMAIN_NAME,
    version: EIP712_DOMAIN_VERSION,
    chainId: snapshot.chainId,
    verifyingContract: snapshot.app as `0x${string}`,
  };
}

export function createSessionKeyGrantMessage(params: {
  owner: `0x${string}`;
  sessionKey: `0x${string}`;
  validUntilTickId: IntegerInput;
  scopes: IntegerInput;
}): SessionKeyGrantMessage {
  return {
    owner: params.owner,
    sessionKey: params.sessionKey,
    validUntilTickId: toBigInt(params.validUntilTickId).toString(),
    scopes: toBigInt(params.scopes).toString(),
  };
}

export function buildSessionKeyGrantTypedData(
  snapshot: SnapshotResponse,
  grant: SessionKeyGrantMessage,
): SessionKeyGrantTypedData {
  return {
    domain: createSequencerDomain(snapshot),
    types: SESSION_KEY_GRANT_TYPES,
    primaryType: SESSION_KEY_GRANT_PRIMARY_TYPE,
    message: {
      owner: grant.owner as `0x${string}`,
      sessionKey: grant.sessionKey as `0x${string}`,
      validUntilTickId: toBigInt(grant.validUntilTickId),
      scopes: toBigInt(grant.scopes),
    },
  };
}

export function hashGrantId(snapshot: SnapshotResponse, grant: SessionKeyGrantMessage): Hex {
  const typedData = buildSessionKeyGrantTypedData(snapshot, grant);
  return hashTypedData(typedData);
}

export async function registerGrant(
  baseUrl: string,
  grant: SessionKeyGrantMessage,
  ownerSig: Hex,
  fetchImpl: FetchLike = fetch,
): Promise<RegisterGrantResponse> {
  const json = await requestJson(
    baseUrl,
    "/v0/register_grant",
    {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        grant,
        ownerSig,
      }),
    },
    fetchImpl,
  );
  return registerGrantResponseSchema.parse(json);
}

export function createTradingEnvelopeMessage(params: {
  owner: `0x${string}`;
  nonce: IntegerInput;
  minInputIndexPlusOne: IntegerInput;
  validUntilTickId: IntegerInput;
  actionBytes: Hex;
  grantId?: Hex;
}): TradingEnvelopeMessage {
  return {
    owner: params.owner,
    nonce: toBigInt(params.nonce).toString(),
    minInputIndexPlusOne: toBigInt(params.minInputIndexPlusOne).toString(),
    validUntilTickId: toBigInt(params.validUntilTickId).toString(),
    actionBytes: params.actionBytes,
    grantId: params.grantId ?? ZERO_GRANT_ID,
  };
}

export function createTradingEnvelopeMessageFromSnapshot(params: {
  snapshot: SnapshotResponse;
  owner: `0x${string}`;
  nonce: IntegerInput;
  validUntilTickId: IntegerInput;
  actionBytes: Hex;
  grantId?: Hex;
}): TradingEnvelopeMessage {
  return createTradingEnvelopeMessage({
    owner: params.owner,
    nonce: params.nonce,
    minInputIndexPlusOne: params.snapshot.minInputIndexPlusOne,
    validUntilTickId: params.validUntilTickId,
    actionBytes: params.actionBytes,
    grantId: params.grantId,
  });
}

export function buildTradingEnvelopeTypedData(
  snapshot: SnapshotResponse,
  envelope: TradingEnvelopeMessage,
): TradingEnvelopeTypedData {
  return {
    domain: createSequencerDomain(snapshot),
    types: TRADING_ENVELOPE_TYPES,
    primaryType: TRADING_ENVELOPE_PRIMARY_TYPE,
    message: {
      owner: envelope.owner as `0x${string}`,
      nonce: toBigInt(envelope.nonce),
      minInputIndexPlusOne: toBigInt(envelope.minInputIndexPlusOne),
      validUntilTickId: toBigInt(envelope.validUntilTickId),
      actionHash: keccak256(envelope.actionBytes),
      grantId: envelope.grantId as Hex,
    },
  };
}

export async function signEnvelopeWithOwner(params: {
  snapshot: SnapshotResponse;
  envelope: TradingEnvelopeMessage;
  signTypedData: (typedData: TradingEnvelopeTypedData) => Promise<Hex>;
}): Promise<Hex> {
  return params.signTypedData(buildTradingEnvelopeTypedData(params.snapshot, params.envelope));
}

export async function signEnvelopeWithSessionKey(params: {
  snapshot: SnapshotResponse;
  envelope: TradingEnvelopeMessage;
  signTypedData: (typedData: TradingEnvelopeTypedData) => Promise<Hex>;
}): Promise<Hex> {
  return params.signTypedData(buildTradingEnvelopeTypedData(params.snapshot, params.envelope));
}

export async function submitEnvelope(
  baseUrl: string,
  params: {
    envelope: TradingEnvelopeMessage;
    ownerSig?: Hex | null;
    sessionSig?: Hex | null;
  },
  fetchImpl: FetchLike = fetch,
): Promise<SubmitEnvelopeReceipt> {
  const json = await requestJson(
    baseUrl,
    "/v0/envelopes",
    {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: JSON.stringify({
        envelope: params.envelope,
        ownerSig: params.ownerSig ?? null,
        sessionSig: params.sessionSig ?? null,
      }),
    },
    fetchImpl,
  );
  return submitReceiptSchema.parse(json);
}

export function createWithdrawCollateralActionBytes(params: WithdrawCollateralActionParams): Hex {
  return encodePacked(
    ["uint8", "uint64", "uint8"],
    [2, toUint64(params.amount, "amount"), toUint8(params.amountExponent ?? 6, "amountExponent")],
  );
}

export function createPlaceOrderActionBytes(params: PlaceOrderActionParams): Hex {
  return encodePacked(
    ["uint8", "uint64", "uint64", "uint8", "uint64", "uint8", "uint8", "uint8", "uint8", "uint8"],
    [
      3,
      toUint64(params.symbol, "symbol"),
      toUint64(params.price, "price"),
      toUint8(params.priceExponent ?? 12, "priceExponent"),
      toUint64(params.size, "size"),
      toUint8(params.sizeExponent ?? 6, "sizeExponent"),
      toUint8(params.side, "side"),
      toUint8(params.timeInForce, "timeInForce"),
      toFlag(params.postOnly ?? false, "postOnly"),
      toFlag(params.reduceOnly ?? false, "reduceOnly"),
    ],
  );
}

export function createCancelOrderActionBytes(params: CancelOrderActionParams): Hex {
  return encodePacked(["uint8", "uint64"], [4, toUint64(params.orderId, "orderId")]);
}

async function requestJson(
  baseUrl: string,
  path: string,
  init: RequestInit,
  fetchImpl: FetchLike,
): Promise<unknown> {
  const response = await fetchImpl(`${trimTrailingSlash(baseUrl)}${path}`, init);
  const contentType = response.headers.get("content-type") ?? "";
  const isJson = contentType.includes("application/json");
  const body = isJson ? await response.json() : await response.text();
  if (!response.ok) {
    const message = extractErrorMessage(body) ?? `HTTP ${response.status}`;
    throw new SequencerClientError(response.status, message, body);
  }
  return body;
}

function extractErrorMessage(value: unknown): string | undefined {
  if (typeof value === "string") {
    return value;
  }
  if (
    value !== null &&
    typeof value === "object" &&
    "error" in value &&
    typeof (value as { error: unknown }).error === "string"
  ) {
    return (value as { error: string }).error;
  }
  return undefined;
}

function trimTrailingSlash(url: string): string {
  return url.endsWith("/") ? url.slice(0, -1) : url;
}

function toBigInt(value: IntegerInput): bigint {
  if (typeof value === "bigint") {
    return value;
  }
  if (typeof value === "number") {
    if (!Number.isInteger(value)) {
      throw new Error("Expected integer numeric value");
    }
    return BigInt(value);
  }
  return BigInt(value);
}

function toUint64(value: IntegerInput, field: string): bigint {
  const parsed = toBigInt(value);
  if (parsed < 0n || parsed > UINT64_MAX) {
    throw new Error(`${field} out of range for uint64`);
  }
  return parsed;
}

function toUint8(value: IntegerInput, field: string): number {
  const parsed = toBigInt(value);
  if (parsed < 0n || parsed > UINT8_MAX) {
    throw new Error(`${field} out of range for uint8`);
  }
  return Number(parsed);
}

function toFlag(value: FlagInput, field: string): number {
  if (typeof value === "boolean") {
    return value ? 1 : 0;
  }
  const parsed = toBigInt(value);
  if (parsed !== 0n && parsed !== 1n) {
    throw new Error(`${field} must be 0 or 1`);
  }
  return Number(parsed);
}
