import { bytesToHex, getAddress, hexToBytes, stringToHex, type Hex } from "viem";

import { HttpError } from "./errors.js";

export function parseBigIntValue(raw: string, field: string): bigint {
  try {
    return BigInt(raw);
  } catch {
    throw new HttpError(400, `Invalid bigint for ${field}`);
  }
}

export function parseNumberValue(raw: string, field: string): number {
  const parsed = Number.parseInt(raw, 10);
  if (!Number.isFinite(parsed)) {
    throw new HttpError(500, `Invalid numeric config for ${field}`);
  }
  return parsed;
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

function canonicalize(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => canonicalize(item));
  }
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>).sort(([a], [b]) =>
      a.localeCompare(b),
    );
    const out: Record<string, unknown> = {};
    for (const [key, innerValue] of entries) {
      out[key] = canonicalize(innerValue);
    }
    return out;
  }
  return value;
}

export function canonicalJsonStringify(value: unknown): string {
  return JSON.stringify(canonicalize(value));
}

export function canonicalJsonToHex(value: unknown): `0x${string}` {
  return stringToHex(canonicalJsonStringify(value));
}

export function normalizeHex(value: string, field: string): Hex {
  try {
    return bytesToHex(hexToBytes(value as Hex));
  } catch {
    throw new HttpError(400, `Invalid hex for ${field}`);
  }
}

export function parseAddress(value: string, field: string): `0x${string}` {
  try {
    return getAddress(value);
  } catch {
    throw new HttpError(400, `Invalid address for ${field}`);
  }
}

export function compareBigIntKeys(a: string, b: string): number {
  const aValue = BigInt(a);
  const bValue = BigInt(b);
  if (aValue < bValue) {
    return -1;
  }
  if (aValue > bValue) {
    return 1;
  }
  return 0;
}
