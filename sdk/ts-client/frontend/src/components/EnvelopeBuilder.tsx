import { useEffect, useMemo, useState } from "react";
import type { Address, Hex, WalletClient } from "viem";
import type { PrivateKeyAccount } from "viem/accounts";

import { apiClient } from "../api/client";
import type { SessionGrantBundle, SnapshotResponse, SubmitEnvelopeResponse } from "../api/types";
import { asAddress, buildTradingEnvelopeTypedData, computeActionHash, parseBigIntInput } from "../eip712/builders";
import { ZERO_GRANT_ID } from "../eip712/types";
import { toErrorMessage } from "../utils/errors";
import { CopyButton } from "./CopyButton";
import { ActionTypeSection, buildActionBytesFromValues, type ActionFormValues } from "./envelopeBuilder/actionSection";
import { normalizeSuggestedNonce, parseNonNegativeBigIntMaybe } from "./envelopeBuilder/nonce";
import { SessionModeSection } from "./envelopeBuilder/sessionModeSection";
import { deriveGrantBundleStatus, deriveSessionReadiness } from "./envelopeBuilder/sessionValidation";
import type { ActionType, Mode } from "./envelopeBuilder/types";

type Props = {
  sequencerBaseUrl: string;
  ownerAddress: Address | null;
  walletClient: WalletClient | null;
  sessionAccount: PrivateKeyAccount | null;
  snapshot: SnapshotResponse | null;
  onFetchSnapshot: () => Promise<void>;
  grantId: string;
  sessionGrantBundle: SessionGrantBundle | null;
  onOpenWindow: (windowId: string) => void;
};

const ACTION_DEFAULTS: ActionFormValues = {
  symbol: "1",
  price: "100000",
  priceExp: "0",
  size: "1000",
  sizeExp: "0",
  side: "0",
  tif: "0",
  postOnly: "0",
  reduceOnly: "0",
  orderId: "1",
  amount: "1000",
  amountExp: "0",
};

export function EnvelopeBuilder({
  sequencerBaseUrl,
  ownerAddress,
  walletClient,
  sessionAccount,
  snapshot,
  onFetchSnapshot,
  grantId,
  sessionGrantBundle,
  onOpenWindow,
}: Props) {
  const [mode, setMode] = useState<Mode>("owner");
  const [actionType, setActionType] = useState<ActionType>("cancel");
  const [actionForm, setActionForm] = useState<ActionFormValues>(ACTION_DEFAULTS);

  const [nonce, setNonce] = useState("1");
  const [minInputIndexPlusOne, setMinInputIndexPlusOne] = useState("0");
  const [validUntilTickId, setValidUntilTickId] = useState("100000");

  const [actionBytes, setActionBytes] = useState<Hex | null>(null);
  const [signature, setSignature] = useState<Hex | null>(null);
  const [submitReceipt, setSubmitReceipt] = useState<SubmitEnvelopeResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"build" | "sign" | "submit" | null>(null);
  const [ensureGrantRegistered, setEnsureGrantRegistered] = useState(true);
  const [suggestedNonce, setSuggestedNonce] = useState<string | null>(null);

  useEffect(() => {
    if (snapshot && minInputIndexPlusOne === "0") {
      setMinInputIndexPlusOne(snapshot.minInputIndexPlusOne);
    }
  }, [snapshot, minInputIndexPlusOne]);

  useEffect(() => {
    if (mode === "session" && actionType === "withdraw") {
      setActionType("cancel");
    }
  }, [mode, actionType]);

  const effectiveGrantId = useMemo(() => {
    if (mode === "owner") {
      return ZERO_GRANT_ID;
    }
    return grantId;
  }, [grantId, mode]);

  const nonceSuggestionStorageKey = useMemo(() => {
    if (!ownerAddress) {
      return null;
    }
    return `sequencer.console.nextNonce:${sequencerBaseUrl.toLowerCase()}:${ownerAddress.toLowerCase()}`;
  }, [sequencerBaseUrl, ownerAddress]);

  useEffect(() => {
    if (!nonceSuggestionStorageKey) {
      setSuggestedNonce(null);
      return;
    }
    const stored = localStorage.getItem(nonceSuggestionStorageKey);
    setSuggestedNonce(normalizeSuggestedNonce(stored));
  }, [nonceSuggestionStorageKey]);

  const grantBundleStatus = useMemo(
    () =>
      deriveGrantBundleStatus({
        mode,
        sessionGrantBundle,
        effectiveGrantId,
        ownerAddress,
        sessionAccount,
      }),
    [mode, sessionGrantBundle, effectiveGrantId, ownerAddress, sessionAccount],
  );

  const sessionReadiness = useMemo(
    () =>
      deriveSessionReadiness({
        mode,
        effectiveGrantId,
        ownerAddress,
        sessionAccount,
        actionType,
        nonce,
        sessionGrantBundle,
      }),
    [mode, effectiveGrantId, ownerAddress, sessionAccount, actionType, nonce, sessionGrantBundle],
  );

  const canPreRegisterGrant =
    mode === "session" &&
    ensureGrantRegistered &&
    grantBundleStatus !== null &&
    grantBundleStatus.tone === "ok" &&
    sessionGrantBundle !== null;

  const onActionFieldChange = <K extends keyof ActionFormValues>(field: K, value: string) => {
    setActionForm((current) => ({ ...current, [field]: value }));
  };

  const onBuildAction = () => {
    setError(null);
    try {
      setBusy("build");
      const encoded = buildActionBytesFromValues(actionType, actionForm);
      setActionBytes(encoded);
      setSignature(null);
      setSubmitReceipt(null);
    } catch (unknownError) {
      setError(toErrorMessage(unknownError));
    } finally {
      setBusy(null);
    }
  };

  const onSign = async () => {
    setError(null);
    setSubmitReceipt(null);

    if (!snapshot) {
      setError("Fetch snapshot first");
      return;
    }
    if (!ownerAddress) {
      setError("Connect owner wallet first");
      return;
    }
    if (!actionBytes) {
      setError("Build actionBytes first");
      return;
    }
    if (mode === "session" && actionType === "withdraw") {
      setError("Session mode cannot sign withdraw action");
      return;
    }

    try {
      setBusy("sign");
      const envelopeMessage = {
        owner: asAddress(ownerAddress, "owner"),
        nonce: parseBigIntInput(nonce, "nonce"),
        minInputIndexPlusOne: parseBigIntInput(minInputIndexPlusOne, "minInputIndexPlusOne"),
        validUntilTickId: parseBigIntInput(validUntilTickId, "validUntilTickId"),
        actionHash: computeActionHash(actionBytes),
        grantId: (mode === "owner" ? ZERO_GRANT_ID : effectiveGrantId) as `0x${string}`,
      };

      const typedData = buildTradingEnvelopeTypedData(snapshot, envelopeMessage);

      let signed: Hex;
      if (mode === "owner") {
        if (!walletClient) {
          throw new Error("Wallet client unavailable; reconnect wallet");
        }
        if (typeof walletClient.signTypedData !== "function") {
          throw new Error("Connected wallet does not expose signTypedData; reconnect and try again");
        }
        signed = await walletClient.signTypedData(typedData);
      } else {
        if (!sessionAccount) {
          throw new Error("Session key missing or invalid");
        }
        if (!/^0x[a-fA-F0-9]{64}$/.test(effectiveGrantId)) {
          throw new Error("Session mode requires a valid grantId");
        }
        signed = await sessionAccount.signTypedData(typedData);
      }

      setSignature(signed);
    } catch (unknownError) {
      setError(toErrorMessage(unknownError));
    } finally {
      setBusy(null);
    }
  };

  const onSubmit = async () => {
    setError(null);
    if (!ownerAddress || !actionBytes || !signature) {
      setError("Build actionBytes and sign envelope first");
      return;
    }
    if (mode === "session" && !sessionReadiness.ready) {
      setError(`Session readiness failed: ${sessionReadiness.reasons.join("; ")}`);
      return;
    }

    try {
      setBusy("submit");
      if (canPreRegisterGrant && sessionGrantBundle) {
        await apiClient.registerGrant(sequencerBaseUrl, {
          grant: sessionGrantBundle.grant,
          ownerSig: sessionGrantBundle.ownerSig,
        });
      }

      const payload = {
        envelope: {
          owner: ownerAddress,
          nonce,
          minInputIndexPlusOne,
          validUntilTickId,
          grantId: (mode === "owner" ? ZERO_GRANT_ID : effectiveGrantId) as `0x${string}`,
          actionBytes,
        },
        ...(mode === "owner" ? { ownerSig: signature } : { sessionSig: signature }),
      };

      const receipt = await apiClient.submitEnvelope(sequencerBaseUrl, payload);
      setSubmitReceipt(receipt);
      if (nonceSuggestionStorageKey) {
        const parsedNonce = parseNonNegativeBigIntMaybe(nonce);
        if (parsedNonce.ok) {
          const next = (parsedNonce.value + 1n).toString();
          localStorage.setItem(nonceSuggestionStorageKey, next);
          setSuggestedNonce(next);
        }
      }
    } catch (unknownError) {
      setError(toErrorMessage(unknownError));
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="card">
      <h3>Envelope Builder</h3>
      <div className="row gap">
        <button type="button" className="btn btn-small" onClick={() => void onFetchSnapshot()}>
          Fetch snapshot
        </button>
        <div className="muted">Uses snapshot for chainId/app and anchor defaults</div>
      </div>

      <div className="field-grid">
        <label>
          Mode
          <select value={mode} onChange={(event) => setMode(event.target.value as Mode)}>
            <option value="owner">owner</option>
            <option value="session">session</option>
          </select>
        </label>

        <label>
          Action type
          <select value={actionType} onChange={(event) => setActionType(event.target.value as ActionType)}>
            <option value="place">place</option>
            <option value="cancel">cancel</option>
            <option value="withdraw" disabled={mode === "session"}>
              withdraw {mode === "session" ? "(owner only)" : ""}
            </option>
          </select>
        </label>
      </div>

      <div className="field-grid">
        <label>
          nonce
          <div className="row gap wrap">
            <input value={nonce} onChange={(event) => setNonce(event.target.value)} />
            <button
              type="button"
              className="btn btn-small"
              onClick={() => {
                const parsed = parseNonNegativeBigIntMaybe(nonce);
                setNonce((parsed.ok ? parsed.value + 1n : 1n).toString());
              }}
            >
              +1
            </button>
            {suggestedNonce ? (
              <button type="button" className="btn btn-small" onClick={() => setNonce(suggestedNonce)}>
                Use suggested
              </button>
            ) : null}
          </div>
          {suggestedNonce ? <div className="muted">Suggested next nonce: {suggestedNonce}</div> : null}
        </label>
        <label>
          minInputIndexPlusOne
          <input value={minInputIndexPlusOne} onChange={(event) => setMinInputIndexPlusOne(event.target.value)} />
        </label>
        <label>
          validUntilTickId
          <input value={validUntilTickId} onChange={(event) => setValidUntilTickId(event.target.value)} />
        </label>
        <label>
          grantId (session mode)
          <input className="mono" value={effectiveGrantId} onChange={() => undefined} readOnly title="Owner mode uses zero grantId" />
        </label>
      </div>

      <ActionTypeSection actionType={actionType} values={actionForm} onFieldChange={onActionFieldChange} />

      <div className="row gap">
        <button type="button" className="btn" onClick={onBuildAction} disabled={busy !== null}>
          Build actionBytes
        </button>
        <button type="button" className="btn" onClick={() => void onSign()} disabled={busy !== null}>
          {busy === "sign" ? "Signing..." : "Sign envelope"}
        </button>
        <button
          type="button"
          className="btn"
          onClick={() => void onSubmit()}
          disabled={busy !== null || !signature || (mode === "session" && !sessionReadiness.ready)}
        >
          {busy === "submit" ? "Submitting..." : "Submit"}
        </button>
      </div>

      {mode === "session" ? (
        <SessionModeSection
          grantBundleStatus={grantBundleStatus}
          ensureGrantRegistered={ensureGrantRegistered}
          canPreRegisterGrant={canPreRegisterGrant}
          sessionReadiness={sessionReadiness}
          onToggleEnsureGrantRegistered={setEnsureGrantRegistered}
        />
      ) : null}

      <div>
        <div className="muted">actionBytes</div>
        <div className="mono inline-with-btn">
          {actionBytes ?? "not built"}
          {actionBytes ? <CopyButton value={actionBytes} /> : null}
        </div>
      </div>

      <div>
        <div className="muted">signature</div>
        <div className="mono inline-with-btn">
          {signature ?? "not signed"}
          {signature ? <CopyButton value={signature} /> : null}
        </div>
      </div>

      {error ? <div className="error-box">{error}</div> : null}

      {submitReceipt ? (
        <div>
          <div className="row gap">
            <h4>Receipt</h4>
            <button type="button" className="btn btn-small" onClick={() => onOpenWindow(submitReceipt.windowId)}>
              Open window in Monitor
            </button>
          </div>
          <pre className="mono pre">{JSON.stringify(submitReceipt, null, 2)}</pre>
        </div>
      ) : null}
    </section>
  );
}
