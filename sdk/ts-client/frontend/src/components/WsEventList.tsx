import { useMemo, useState } from "react";

import { asRecord } from "../utils/object";

type WsEvent = {
  receivedAtMs: number;
  raw: unknown;
};

type Props = {
  events: WsEvent[];
  onOpenWindow: (windowId: string) => void;
};

export function WsEventList({ events, onOpenWindow }: Props) {
  const [expanded, setExpanded] = useState<Record<number, boolean>>({});

  const sorted = useMemo(() => [...events].reverse(), [events]);

  return (
    <section className="card">
      <h3>Events ({events.length})</h3>
      <div className="ws-list">
        {sorted.map((event, idx) => {
          const payload = asRecord(event.raw);
          const kind = typeof payload?.kind === "string" ? payload.kind : "-";
          const windowId = typeof payload?.windowId === "string" ? payload.windowId : null;
          const owner = typeof payload?.owner === "string" ? payload.owner : null;
          const grantId = typeof payload?.grantId === "string" ? payload.grantId : null;
          const summary = [
            new Date(event.receivedAtMs).toLocaleTimeString(),
            `kind=${kind}`,
            windowId ? `window=${windowId}` : null,
            owner ? `owner=${short(owner)}` : null,
            grantId ? `grant=${short(grantId)}` : null,
          ]
            .filter((part): part is string => part !== null)
            .join("  ");

          const key = sorted.length - idx;
          return (
            <div key={key} className="ws-row">
              <div className="row gap wrap">
                <span className="mono">{summary}</span>
                {windowId ? (
                  <button type="button" className="btn btn-small" onClick={() => onOpenWindow(windowId)}>
                    Open window
                  </button>
                ) : null}
                <button
                  type="button"
                  className="btn btn-small"
                  onClick={() => setExpanded((prev) => ({ ...prev, [key]: !prev[key] }))}
                >
                  {expanded[key] ? "Hide JSON" : "Show JSON"}
                </button>
              </div>
              {expanded[key] ? <pre className="mono pre">{JSON.stringify(event.raw, null, 2)}</pre> : null}
            </div>
          );
        })}
      </div>
    </section>
  );
}

function short(value: string): string {
  if (value.length <= 16) {
    return value;
  }
  return `${value.slice(0, 10)}...${value.slice(-4)}`;
}
