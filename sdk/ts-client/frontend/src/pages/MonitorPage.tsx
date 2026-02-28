import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";

import { apiClient } from "../api/client";
import type {
  PendingWindowSummary,
  PendingWindowsDetailResponse,
  PendingWindowsSummaryResponse,
  WindowIntrospection,
} from "../api/types";
import { DebugControls, type MonitorDebugOptions } from "../components/DebugControls";
import { PendingList } from "../components/PendingList";
import { WindowDetail } from "../components/WindowDetail";
import { useSequencerSettings } from "../state/sequencerUrl";

export function MonitorPage() {
  const { sequencerBaseUrl, autoRefresh, refreshIntervalMs, lastFlush } = useSequencerSettings();
  const [searchParams, setSearchParams] = useSearchParams();
  const [selectedWindowId, setSelectedWindowId] = useState<string | null>(searchParams.get("windowId"));
  const [debug, setDebug] = useState<MonitorDebugOptions>({
    includeBytes: false,
    includeSigs: false,
    includeDetails: false,
    limitEnvelopes: 200,
  });

  useEffect(() => {
    const queryWindowId = searchParams.get("windowId");
    setSelectedWindowId(queryWindowId);
  }, [searchParams]);

  const polling = autoRefresh ? refreshIntervalMs : false;

  const statusQuery = useQuery({
    queryKey: ["monitor", "status", sequencerBaseUrl],
    queryFn: () => apiClient.getStatus(sequencerBaseUrl),
    refetchInterval: polling,
  });

  const snapshotQuery = useQuery({
    queryKey: ["monitor", "snapshot", sequencerBaseUrl],
    queryFn: () => apiClient.getSnapshot(sequencerBaseUrl),
    refetchInterval: polling,
  });

  const activeQuery = useQuery({
    queryKey: [
      "monitor",
      "active",
      sequencerBaseUrl,
      debug.includeBytes,
      debug.includeSigs,
      debug.limitEnvelopes,
    ],
    queryFn: () =>
      apiClient.getActiveWindow(sequencerBaseUrl, {
        includeBytes: debug.includeBytes,
        includeSigs: debug.includeSigs,
        limitEnvelopes: debug.limitEnvelopes,
      }),
    refetchInterval: polling,
  });

  const pendingQuery = useQuery({
    queryKey: [
      "monitor",
      "pending",
      sequencerBaseUrl,
      debug.includeDetails,
      debug.includeBytes,
      debug.includeSigs,
      debug.limitEnvelopes,
    ],
    queryFn: () =>
      apiClient.getPendingWindows(sequencerBaseUrl, {
        includeDetails: debug.includeDetails,
        includeBytes: debug.includeBytes,
        includeSigs: debug.includeSigs,
        limitEnvelopes: debug.limitEnvelopes,
      }),
    refetchInterval: polling,
  });

  const pendingSummaries = useMemo(() => {
    if (!pendingQuery.data) {
      return [];
    }
    return toPendingSummaries(pendingQuery.data);
  }, [pendingQuery.data]);

  const selectedWindowQuery = useQuery<WindowIntrospection>({
    queryKey: [
      "monitor",
      "window",
      sequencerBaseUrl,
      selectedWindowId,
      debug.includeBytes,
      debug.includeSigs,
      debug.limitEnvelopes,
    ],
    queryFn: () =>
      apiClient.getWindowById(sequencerBaseUrl, selectedWindowId!, {
        includeBytes: debug.includeBytes,
        includeSigs: debug.includeSigs,
        limitEnvelopes: debug.limitEnvelopes,
      }),
    enabled: selectedWindowId !== null,
    refetchInterval: polling,
  });

  const onSelectWindow = (windowId: string) => {
    setSelectedWindowId(windowId);
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      next.set("windowId", windowId);
      return next;
    });
  };

  return (
    <div className="page-grid">
      <section className="card">
        <h2>Monitor</h2>
        <div className="kpi-grid">
          <div>
            <div className="muted">Active window</div>
            <div className="mono">{activeQuery.data?.windowId ?? "-"}</div>
            <div>
              ageMs {activeQuery.data?.ageMs ?? "-"} | count {activeQuery.data?.count ?? 0} | bytes {activeQuery.data?.bytes ?? 0}
            </div>
          </div>
          <div>
            <div className="muted">Pending windows</div>
            <div>{pendingSummaries.length}</div>
          </div>
          <div>
            <div className="muted">Snapshot anchor</div>
            <div>minInput+1: {snapshotQuery.data?.minInputIndexPlusOne ?? "-"}</div>
            <div>chainId: {snapshotQuery.data?.chainId ?? "-"}</div>
            <div className="mono">app: {snapshotQuery.data?.app ?? "-"}</div>
          </div>
          <div>
            <div className="muted">Last flush</div>
            {lastFlush ? (
              <div>
                <div>{new Date(lastFlush.atMs).toLocaleTimeString()}</div>
                <div>
                  window {lastFlush.response.windowId} | count {lastFlush.response.count}
                </div>
                <div>{lastFlush.response.flushed ? lastFlush.response.status : "NOOP"}</div>
              </div>
            ) : (
              <div>-</div>
            )}
          </div>
        </div>

        {(statusQuery.error || snapshotQuery.error || activeQuery.error || pendingQuery.error) && (
          <div className="error-box">
            {[statusQuery.error, snapshotQuery.error, activeQuery.error, pendingQuery.error]
              .filter((error): error is Error => error instanceof Error)
              .map((error) => error.message)
              .join(" | ")}
          </div>
        )}
      </section>

      <DebugControls value={debug} onChange={setDebug} />

      <div className="split-2">
        <WindowDetail title="Active Window" window={activeQuery.data ?? null} />

        <div className="stack">
          <PendingList windows={pendingSummaries} selectedWindowId={selectedWindowId} onSelect={onSelectWindow} />
          <WindowDetail title="Selected Pending Window" window={selectedWindowQuery.data ?? null} />
          {selectedWindowQuery.error ? <div className="error-box">{(selectedWindowQuery.error as Error).message}</div> : null}
        </div>
      </div>
    </div>
  );
}

function toPendingSummaries(
  data: PendingWindowsSummaryResponse | PendingWindowsDetailResponse,
): PendingWindowSummary[] {
  const windows = data.windows;
  if (windows.length === 0) {
    return [];
  }

  const first = windows[0] as PendingWindowSummary | WindowIntrospection;
  if ("grantCount" in first) {
    return windows as PendingWindowSummary[];
  }

  return (windows as WindowIntrospection[]).map((window) => ({
    windowId: window.windowId,
    state: window.state === "ACTIVE" ? "BUFFERED" : window.state,
    count: window.count,
    bytes: window.bytes,
    grantCount: window.grants.length,
    ...(window.txHash ? { txHash: window.txHash } : {}),
    ...(window.postedAtMs ? { postedAtMs: window.postedAtMs } : {}),
  }));
}
