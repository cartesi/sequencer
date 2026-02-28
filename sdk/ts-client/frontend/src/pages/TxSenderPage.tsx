import { useState } from "react";
import { useAccount, useWalletClient } from "wagmi";
import { useNavigate } from "react-router-dom";

import { apiClient } from "../api/client";
import type { SnapshotResponse } from "../api/types";
import { EnvelopeBuilder } from "../components/EnvelopeBuilder";
import { GrantPanel } from "../components/GrantPanel";
import { SessionKeyPanel } from "../components/SessionKeyPanel";
import { WalletBar } from "../components/WalletBar";
import { CopyButton } from "../components/CopyButton";
import { useDerivedSessionAccount } from "../hooks/useDerivedSessionAccount";
import { useGrantPersistence } from "../hooks/useGrantPersistence";
import { useSequencerSettings } from "../state/sequencerUrl";
import { useTxSenderState } from "../state/txSender";
import { toErrorMessage } from "../utils/errors";

export function TxSenderPage() {
  const { sequencerBaseUrl } = useSequencerSettings();
  const navigate = useNavigate();
  const { address } = useAccount();
  const { data: walletClientData } = useWalletClient();
  const walletClient = walletClientData ?? null;
  const { sessionPrivateKey, setSessionPrivateKey } = useTxSenderState();
  const [snapshot, setSnapshot] = useState<SnapshotResponse | null>(null);
  const [snapshotError, setSnapshotError] = useState<string | null>(null);
  const [fetchingSnapshot, setFetchingSnapshot] = useState(false);

  const sessionAccount = useDerivedSessionAccount(sessionPrivateKey);
  const grantPersistence = useGrantPersistence({
    sequencerBaseUrl,
    ownerAddress: address ?? null,
    sessionAddress: sessionAccount?.address ?? null,
  });

  const fetchSnapshot = async () => {
    setSnapshotError(null);
    try {
      setFetchingSnapshot(true);
      const next = await apiClient.getSnapshot(sequencerBaseUrl);
      setSnapshot(next);
    } catch (unknownError) {
      setSnapshotError(toErrorMessage(unknownError));
    } finally {
      setFetchingSnapshot(false);
    }
  };

  const openWindowInMonitor = (windowId: string) => {
    navigate(`/monitor?windowId=${encodeURIComponent(windowId)}`);
  };

  return (
    <div className="page-grid">
      <section className="card">
        <h2>Tx Sender</h2>
        <div className="row gap">
          <button type="button" className="btn" onClick={() => void fetchSnapshot()} disabled={fetchingSnapshot}>
            {fetchingSnapshot ? "Fetching snapshot..." : "Fetch snapshot"}
          </button>
          {snapshot ? (
            <div className="row gap wrap">
              <span>chainId {snapshot.chainId}</span>
              <span>minInput+1 {snapshot.minInputIndexPlusOne}</span>
              <span className="mono inline-with-btn">
                app {snapshot.app}
                <CopyButton value={snapshot.app} />
              </span>
            </div>
          ) : (
            <span className="muted">No snapshot loaded yet</span>
          )}
        </div>
        {snapshotError ? <div className="error-box">{snapshotError}</div> : null}
      </section>

      <WalletBar snapshotChainId={snapshot?.chainId ?? null} />

      <div className="split-2">
        <div className="stack">
          <SessionKeyPanel sessionPrivateKey={sessionPrivateKey} onChangePrivateKey={setSessionPrivateKey} />
          <GrantPanel
            sequencerBaseUrl={sequencerBaseUrl}
            ownerAddress={address ?? null}
            sessionAddress={sessionAccount?.address ?? null}
            walletClient={walletClient}
            snapshot={snapshot}
            onFetchSnapshot={fetchSnapshot}
            grantPersistence={grantPersistence}
          />
        </div>

        <EnvelopeBuilder
          sequencerBaseUrl={sequencerBaseUrl}
          ownerAddress={address ?? null}
          walletClient={walletClient}
          sessionAccount={sessionAccount}
          snapshot={snapshot}
          onFetchSnapshot={fetchSnapshot}
          grantId={grantPersistence.grantId}
          sessionGrantBundle={grantPersistence.sessionGrantBundle}
          onOpenWindow={openWindowInMonitor}
        />
      </div>
    </div>
  );
}
