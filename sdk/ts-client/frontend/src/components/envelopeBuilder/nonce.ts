export function parseNonNegativeBigIntMaybe(raw: string): { ok: true; value: bigint } | { ok: false } {
  const normalized = raw.trim();
  if (normalized.length === 0) {
    return { ok: false };
  }
  try {
    const parsed = BigInt(normalized);
    if (parsed < 0n) {
      return { ok: false };
    }
    return { ok: true, value: parsed };
  } catch {
    return { ok: false };
  }
}

export function normalizeSuggestedNonce(raw: string | null): string | null {
  if (raw === null) {
    return null;
  }
  const parsed = parseNonNegativeBigIntMaybe(raw);
  return parsed.ok ? parsed.value.toString() : null;
}
