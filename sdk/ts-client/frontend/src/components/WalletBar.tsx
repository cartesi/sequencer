import { useAccount, useChainId, useConnect, useDisconnect } from "wagmi";

export function WalletBar({ snapshotChainId }: { snapshotChainId: number | null }) {
  const { address, isConnected } = useAccount();
  const { connect, connectors, isPending } = useConnect();
  const { disconnect } = useDisconnect();
  const walletChainId = useChainId();

  return (
    <section className="card">
      <h3>Wallet</h3>
      <div className="row gap">
        <div>
          <div className="muted">Owner address</div>
          <div className="mono">{isConnected ? address : "Not connected"}</div>
        </div>
        <div>
          <div className="muted">Wallet chainId</div>
          <div>{isConnected ? walletChainId : "-"}</div>
        </div>
      </div>

      {isConnected && snapshotChainId !== null && snapshotChainId !== walletChainId ? (
        <div className="error-box">
          Wallet chainId ({walletChainId}) does not match snapshot.chainId ({snapshotChainId}).
        </div>
      ) : null}

      <div className="row gap">
        {!isConnected ? (
          <button
            type="button"
            className="btn"
            disabled={isPending || connectors.length === 0}
            onClick={() => {
              const connector = connectors[0];
              if (connector) {
                connect({ connector });
              }
            }}
          >
            {isPending ? "Connecting..." : "Connect Wallet"}
          </button>
        ) : (
          <button type="button" className="btn" onClick={() => disconnect()}>
            Disconnect
          </button>
        )}
      </div>
    </section>
  );
}
