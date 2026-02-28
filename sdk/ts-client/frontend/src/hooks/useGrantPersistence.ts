import { useEffect, useMemo, useState, type Dispatch, type SetStateAction } from "react";
import type { Address } from "viem";

import type { SessionGrantBundle } from "../api/types";

type UseGrantPersistenceArgs = {
  sequencerBaseUrl: string;
  ownerAddress: Address | null;
  sessionAddress: Address | null;
};

export type GrantPersistence = {
  grantId: string;
  setGrantId: Dispatch<SetStateAction<string>>;
  sessionGrantBundle: SessionGrantBundle | null;
  setSessionGrantBundle: Dispatch<SetStateAction<SessionGrantBundle | null>>;
};

export function useGrantPersistence(args: UseGrantPersistenceArgs): GrantPersistence {
  const { sequencerBaseUrl, ownerAddress, sessionAddress } = args;

  const grantStorageKey = useMemo(
    () =>
      `sequencer.console.lastGrantId:${sequencerBaseUrl.toLowerCase()}:${(ownerAddress ?? "none").toLowerCase()}:${(sessionAddress ?? "none").toLowerCase()}`,
    [sequencerBaseUrl, ownerAddress, sessionAddress],
  );
  const grantBundleStorageKey = `${grantStorageKey}:bundle`;

  const [grantId, setGrantId] = useState("");
  const [sessionGrantBundle, setSessionGrantBundle] = useState<SessionGrantBundle | null>(null);
  const [hydratedKey, setHydratedKey] = useState<string | null>(null);

  useEffect(() => {
    const storedGrantId = localStorage.getItem(grantStorageKey) ?? "";
    const raw = localStorage.getItem(grantBundleStorageKey);
    let nextBundle: SessionGrantBundle | null = null;
    if (!raw) {
      nextBundle = null;
    } else {
      try {
        const parsed = JSON.parse(raw);
        if (isSessionGrantBundle(parsed)) {
          nextBundle = parsed;
        }
      } catch {
        // no-op
      }
    }
    setGrantId(storedGrantId);
    setSessionGrantBundle(nextBundle);
    setHydratedKey(grantStorageKey);
  }, [grantStorageKey, grantBundleStorageKey]);

  useEffect(() => {
    if (hydratedKey !== grantStorageKey) {
      return;
    }
    if (grantId.trim().length > 0) {
      localStorage.setItem(grantStorageKey, grantId);
      return;
    }
    localStorage.removeItem(grantStorageKey);
  }, [grantStorageKey, grantId, hydratedKey]);

  useEffect(() => {
    if (hydratedKey !== grantStorageKey) {
      return;
    }
    if (sessionGrantBundle) {
      localStorage.setItem(grantBundleStorageKey, JSON.stringify(sessionGrantBundle));
      return;
    }
    localStorage.removeItem(grantBundleStorageKey);
  }, [grantBundleStorageKey, grantStorageKey, sessionGrantBundle, hydratedKey]);

  return {
    grantId,
    setGrantId,
    sessionGrantBundle,
    setSessionGrantBundle,
  };
}

function isSessionGrantBundle(value: unknown): value is SessionGrantBundle {
  if (!value || typeof value !== "object") {
    return false;
  }
  const candidate = value as SessionGrantBundle;
  return (
    typeof candidate.grantId === "string" &&
    candidate.grant !== undefined &&
    typeof candidate.grant.owner === "string" &&
    typeof candidate.grant.sessionKey === "string" &&
    typeof candidate.grant.validUntilTickId === "string" &&
    typeof candidate.grant.scopes === "string" &&
    typeof candidate.ownerSig === "string"
  );
}
