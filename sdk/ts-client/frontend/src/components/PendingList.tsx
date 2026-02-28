import type { PendingWindowSummary } from "../api/types";

type Props = {
  windows: PendingWindowSummary[];
  selectedWindowId: string | null;
  onSelect: (windowId: string) => void;
};

export function PendingList({ windows, selectedWindowId, onSelect }: Props) {
  return (
    <section className="card">
      <h3>Pending Windows ({windows.length})</h3>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>windowId</th>
              <th>state</th>
              <th>count</th>
              <th>bytes</th>
              <th>grantCount</th>
              <th>txHash</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {windows.length === 0 ? (
              <tr>
                <td colSpan={7} className="muted">
                  none
                </td>
              </tr>
            ) : (
              windows.map((window) => (
                <tr
                  key={window.windowId}
                  className={selectedWindowId === window.windowId ? "row-selected" : undefined}
                >
                  <td className="mono">{window.windowId}</td>
                  <td>{window.state}</td>
                  <td>{window.count}</td>
                  <td>{window.bytes}</td>
                  <td>{window.grantCount}</td>
                  <td className="mono">{window.txHash ? `${window.txHash.slice(0, 12)}...` : "-"}</td>
                  <td>
                    <button type="button" className="btn btn-small" onClick={() => onSelect(window.windowId)}>
                      Open
                    </button>
                  </td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </section>
  );
}
