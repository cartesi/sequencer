import { useMutation, useQueryClient } from "@tanstack/react-query";
import { NavLink, Outlet } from "react-router-dom";

import { apiClient } from "../api/client";
import { useSequencerSettings } from "../state/sequencerUrl";
import { toErrorMessage } from "../utils/errors";

export function Shell() {
  const {
    sequencerBaseUrl,
    setSequencerBaseUrl,
    wsUrl,
    setWsUrl,
    autoRefresh,
    setAutoRefresh,
    refreshIntervalMs,
    setRefreshIntervalMs,
    setLastFlush,
  } = useSequencerSettings();

  const queryClient = useQueryClient();
  const flushMutation = useMutation({
    mutationFn: () => apiClient.flushOldest(sequencerBaseUrl),
    onSuccess: (response) => {
      setLastFlush({ atMs: Date.now(), response });
      void queryClient.invalidateQueries({ queryKey: ["monitor"] });
    },
  });

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="row gap wrap">
          <label>
            Sequencer URL
            <input value={sequencerBaseUrl} onChange={(event) => setSequencerBaseUrl(event.target.value)} />
          </label>

          <label>
            WS URL
            <input value={wsUrl} onChange={(event) => setWsUrl(event.target.value)} />
          </label>

          <label className="inline-check">
            <input type="checkbox" checked={autoRefresh} onChange={(event) => setAutoRefresh(event.target.checked)} />
            Auto refresh
          </label>

          <label>
            Interval
            <select
              value={refreshIntervalMs}
              onChange={(event) => setRefreshIntervalMs(Number.parseInt(event.target.value, 10))}
            >
              <option value={1000}>1s</option>
              <option value={2000}>2s</option>
              <option value={5000}>5s</option>
            </select>
          </label>

          <button
            type="button"
            className="btn"
            onClick={() => void queryClient.invalidateQueries({ queryKey: ["monitor"] })}
          >
            Refresh now
          </button>

          <button
            type="button"
            className="btn"
            disabled={flushMutation.isPending}
            onClick={() => void flushMutation.mutateAsync()}
          >
            {flushMutation.isPending ? "Flushing..." : "Flush oldest"}
          </button>
        </div>

        {flushMutation.error ? (
          <div className="error-box">{toErrorMessage(flushMutation.error)}</div>
        ) : null}
      </header>

      <div className="body-layout">
        <aside className="sidebar">
          <h2>Sequencer Console</h2>
          <nav className="nav-list">
            <NavLink to="/monitor" className={({ isActive }) => (isActive ? "active" : "")}>Monitor</NavLink>
            <NavLink to="/tx-sender" className={({ isActive }) => (isActive ? "active" : "")}>Tx Sender</NavLink>
            <NavLink to="/ws-feed" className={({ isActive }) => (isActive ? "active" : "")}>WS Feed</NavLink>
          </nav>
        </aside>

        <main className="content">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
