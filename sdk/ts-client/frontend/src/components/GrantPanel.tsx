import { useEffect, useState } from "react";
import type { Address, Hex, WalletClient } from "viem";

import { apiClient } from "../api/client";
import type { SessionGrantBundle, SnapshotResponse } from "../api/types";
import { buildGrantTypedData, hashGrantId, parseBigIntInput } from "../eip712/builders";
import type { GrantPersistence } from "../hooks/useGrantPersistence";
import { toErrorMessage } from "../utils/errors";
import { CopyButton } from "./CopyButton";

type Props = {
  sequencerBaseUrl: string;
  ownerAddress: Address | null;
  sessionAddress: Address | null;
  walletClient: WalletClient | null;
  snapshot: SnapshotResponse | null;
  onFetchSnapshot: () => Promise<void>;
  grantPersistence: GrantPersistence;
};

export function GrantPanel({
  sequencerBaseUrl,
  ownerAddress,
  sessionAddress,
  walletClient,
  snapshot,
  onFetchSnapshot,
  grantPersistence,
}: Props) {
  const { grantId, setGrantId, sessionGrantBundle, setSessionGrantBundle } = grantPersistence;
  const [validUntilTickId, setValidUntilTickId] = useState("100000");
  const [scopes, setScopes] = useState("3");
  const [ownerSig, setOwnerSig] = useState<Hex | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"sign" | "register" | null>(null);
  const [registerResponse, setRegisterResponse] = useState<string>("");

  useEffect(() => {
    if (!sessionGrantBundle) {
      return;
    }
    setOwnerSig(sessionGrantBundle.ownerSig);
    setValidUntilTickId(sessionGrantBundle.grant.validUntilTickId);
    setScopes(sessionGrantBundle.grant.scopes);
  }, [sessionGrantBundle]);

  const signGrant = async () => {
    setError(null);
    setRegisterResponse("");
    if (!ownerAddress) {
      setError("Connect owner wallet first");
      return;
    }
    if (!sessionAddress) {
      setError("Set a valid session private key first");
      return;
    }
    if (!walletClient) {
      setError("Wallet client unavailable; reconnect wallet");
      return;
    }
    if (typeof walletClient.signTypedData !== "function") {
      setError("Connected wallet does not expose signTypedData; reconnect and try again");
      return;
    }
    if (!snapshot) {
      setError("Fetch snapshot first");
      return;
    }

    try {
      setBusy("sign");
      const grantMessage = {
        owner: ownerAddress,
        sessionKey: sessionAddress,
        validUntilTickId: parseBigIntInput(validUntilTickId, "validUntilTickId"),
        scopes: parseBigIntInput(scopes, "scopes"),
      };

      const signature = await walletClient.signTypedData(buildGrantTypedData(snapshot, grantMessage));
      const computedGrantId = hashGrantId(snapshot, grantMessage);
      const bundle: SessionGrantBundle = {
        grant: {
          owner: ownerAddress,
          sessionKey: sessionAddress,
          validUntilTickId: grantMessage.validUntilTickId.toString(),
          scopes: grantMessage.scopes.toString(),
        },
        ownerSig: signature,
        grantId: computedGrantId,
      };

      setOwnerSig(signature);
      setGrantId(computedGrantId);
      setSessionGrantBundle(bundle);
    } catch (unknownError) {
      setError(toErrorMessage(unknownError));
    } finally {
      setBusy(null);
    }
  };

  const registerGrant = async () => {
    setError(null);
    setRegisterResponse("");
    if (!ownerAddress || !sessionAddress || !ownerSig) {
      setError("Sign grant first");
      return;
    }

    try {
      setBusy("register");
      const response = await apiClient.registerGrant(sequencerBaseUrl, {
        grant: {
          owner: ownerAddress,
          sessionKey: sessionAddress,
          validUntilTickId,
          scopes,
        },
        ownerSig,
      });
      setRegisterResponse(JSON.stringify(response, null, 2));
      setGrantId(response.grantId);
      setSessionGrantBundle({
        grant: {
          owner: ownerAddress,
          sessionKey: sessionAddress,
          validUntilTickId,
          scopes,
        },
        ownerSig,
        grantId: response.grantId,
      });
    } catch (unknownError) {
      setError(toErrorMessage(unknownError));
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="card">
      <h3>Grant Panel</h3>
      <div className="row gap">
        <button type="button" className="btn btn-small" onClick={() => void onFetchSnapshot()}>
          Fetch snapshot
        </button>
        <div className="muted">Snapshot chain/app used for EIP-712 domain</div>
      </div>

      <div className="field-grid">
        <label>
          validUntilTickId
          <input value={validUntilTickId} onChange={(event) => setValidUntilTickId(event.target.value)} />
        </label>

        <label>
          scopes
          <input value={scopes} onChange={(event) => setScopes(event.target.value)} />
        </label>
      </div>

      <div className="row gap">
        <button type="button" className="btn" onClick={() => void signGrant()} disabled={busy !== null}>
          {busy === "sign" ? "Signing..." : "Sign grant"}
        </button>

        <button type="button" className="btn" onClick={() => void registerGrant()} disabled={busy !== null || !ownerSig}>
          {busy === "register" ? "Registering..." : "Register grant"}
        </button>
      </div>

      <label>
        grantId
        <div className="inline-with-btn">
          <input className="mono" value={grantId} onChange={(event) => setGrantId(event.target.value)} placeholder="0x..." />
          {/^0x[a-fA-F0-9]{64}$/.test(grantId) ? <CopyButton value={grantId} /> : null}
        </div>
      </label>

      <div>
        <div className="muted">ownerSig</div>
        <div className="mono inline-with-btn">
          {ownerSig ?? "Not signed"}
          {ownerSig ? <CopyButton value={ownerSig} /> : null}
        </div>
      </div>

      {error ? <div className="error-box">{error}</div> : null}
      {registerResponse ? <pre className="mono pre">{registerResponse}</pre> : null}
    </section>
  );
}
