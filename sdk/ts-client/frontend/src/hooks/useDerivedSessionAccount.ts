import { useMemo } from "react";
import type { Hex } from "viem";
import { privateKeyToAccount, type PrivateKeyAccount } from "viem/accounts";

export function useDerivedSessionAccount(privateKey: string): PrivateKeyAccount | null {
  return useMemo(() => {
    const candidate = privateKey.trim();
    if (!/^0x[a-fA-F0-9]{64}$/.test(candidate)) {
      return null;
    }
    try {
      return privateKeyToAccount(candidate as Hex);
    } catch {
      return null;
    }
  }, [privateKey]);
}
