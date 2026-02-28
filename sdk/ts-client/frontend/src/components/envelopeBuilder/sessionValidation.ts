import type { Address } from "viem";
import type { PrivateKeyAccount } from "viem/accounts";

import type { SessionGrantBundle } from "../../api/types";
import { parseNonNegativeBigIntMaybe } from "./nonce";
import type { ActionType, Mode } from "./types";

export type GrantBundleStatus = {
  label: string;
  tone: "ok" | "warn";
  detail: string;
} | null;

export type SessionReadiness = {
  ready: boolean;
  reasons: string[];
};

export function deriveGrantBundleStatus(args: {
  mode: Mode;
  sessionGrantBundle: SessionGrantBundle | null;
  effectiveGrantId: string;
  ownerAddress: Address | null;
  sessionAccount: PrivateKeyAccount | null;
}): GrantBundleStatus {
  const { mode, sessionGrantBundle, effectiveGrantId, ownerAddress, sessionAccount } = args;
  if (mode !== "session") {
    return null;
  }
  if (!sessionGrantBundle) {
    return {
      label: "NO_BUNDLE",
      tone: "warn",
      detail: "No signed grant bundle is loaded in the frontend state.",
    };
  }
  if (sessionGrantBundle.grantId.toLowerCase() !== effectiveGrantId.toLowerCase()) {
    return {
      label: "GRANT_ID_MISMATCH",
      tone: "warn",
      detail: "Signed bundle grantId differs from current session grantId.",
    };
  }
  if (!ownerAddress) {
    return {
      label: "OWNER_MISSING",
      tone: "warn",
      detail: "Connect the owner wallet to validate bundle ownership.",
    };
  }
  if (sessionGrantBundle.grant.owner.toLowerCase() !== ownerAddress.toLowerCase()) {
    return {
      label: "OWNER_MISMATCH",
      tone: "warn",
      detail: "Signed grant bundle owner differs from connected owner wallet.",
    };
  }
  if (!sessionAccount) {
    return {
      label: "SESSION_KEY_MISSING",
      tone: "warn",
      detail: "Set a valid session private key to validate the signed bundle.",
    };
  }
  if (sessionGrantBundle.grant.sessionKey.toLowerCase() !== sessionAccount.address.toLowerCase()) {
    return {
      label: "SESSION_KEY_MISMATCH",
      tone: "warn",
      detail: "Signed bundle session key differs from current session private key.",
    };
  }
  return {
    label: "BUNDLE_MATCHED",
    tone: "ok",
    detail: "Signed bundle matches current owner, session key, and grantId.",
  };
}

export function deriveSessionReadiness(args: {
  mode: Mode;
  effectiveGrantId: string;
  ownerAddress: Address | null;
  sessionAccount: PrivateKeyAccount | null;
  actionType: ActionType;
  nonce: string;
  sessionGrantBundle: SessionGrantBundle | null;
}): SessionReadiness {
  const { mode, effectiveGrantId, ownerAddress, sessionAccount, actionType, nonce, sessionGrantBundle } = args;
  if (mode !== "session") {
    return { ready: true, reasons: [] };
  }

  const reasons: string[] = [];
  if (!/^0x[a-fA-F0-9]{64}$/.test(effectiveGrantId)) {
    reasons.push("grantId must be a 32-byte hex value");
  }
  if (!ownerAddress) {
    reasons.push("owner wallet is not connected");
  }
  if (!sessionAccount) {
    reasons.push("session key is missing or invalid");
  }
  if (actionType === "withdraw") {
    reasons.push("withdraw is owner-only and not allowed in session mode");
  }
  if (!parseNonNegativeBigIntMaybe(nonce).ok) {
    reasons.push("nonce must be a non-negative integer");
  }
  if (!sessionGrantBundle) {
    reasons.push("signed grant bundle is missing");
  } else {
    if (sessionGrantBundle.grantId.toLowerCase() !== effectiveGrantId.toLowerCase()) {
      reasons.push("signed bundle grantId does not match current grantId");
    }
    if (ownerAddress && sessionGrantBundle.grant.owner.toLowerCase() !== ownerAddress.toLowerCase()) {
      reasons.push("signed bundle owner does not match connected owner wallet");
    }
    if (sessionAccount && sessionGrantBundle.grant.sessionKey.toLowerCase() !== sessionAccount.address.toLowerCase()) {
      reasons.push("signed bundle session key does not match current session key");
    }
  }

  return { ready: reasons.length === 0, reasons };
}
