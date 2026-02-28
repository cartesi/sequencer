import { useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";

import { WsEventList } from "../components/WsEventList";
import { useSequencerSettings } from "../state/sequencerUrl";
import { toErrorMessage } from "../utils/errors";
import { tryParseJson } from "../utils/json";
import { asRecord } from "../utils/object";

type FeedEvent = {
  receivedAtMs: number;
  raw: unknown;
};

export function WsFeedPage() {
  const { wsUrl } = useSequencerSettings();
  const navigate = useNavigate();
  const socketRef = useRef<WebSocket | null>(null);

  const [events, setEvents] = useState<FeedEvent[]>([]);
  const [connected, setConnected] = useState(false);
  const [connectError, setConnectError] = useState<string | null>(null);

  const [showEnvelopeAccepted, setShowEnvelopeAccepted] = useState(true);
  const [showDirect, setShowDirect] = useState(true);
  const [hideSequencerBatchDirect, setHideSequencerBatchDirect] = useState(true);
  const [search, setSearch] = useState("");

  const connect = () => {
    setConnectError(null);
    if (socketRef.current) {
      socketRef.current.close();
    }

    try {
      const socket = new WebSocket(wsUrl);
      socketRef.current = socket;

      socket.onopen = () => setConnected(true);
      socket.onclose = () => setConnected(false);
      socket.onerror = () => setConnectError("WebSocket error");
      socket.onmessage = (message) => {
        const raw = tryParseJson(message.data);
        setEvents((prev) => [{ receivedAtMs: Date.now(), raw }, ...prev].slice(0, 500));
      };
    } catch (unknownError) {
      setConnectError(toErrorMessage(unknownError));
    }
  };

  const disconnect = () => {
    if (socketRef.current) {
      socketRef.current.close();
      socketRef.current = null;
    }
    setConnected(false);
  };

  const filteredEvents = useMemo(() => {
    const term = search.trim().toLowerCase();

    return events.filter((event) => {
      const payload = asRecord(event.raw);
      const kind = typeof payload?.kind === "string" ? payload.kind : null;
      const directSource = typeof payload?.directSource === "string" ? payload.directSource : null;

      if (kind === "ENVELOPE_ACCEPTED" && !showEnvelopeAccepted) {
        return false;
      }
      if (kind === "DIRECT") {
        if (!showDirect) {
          return false;
        }
        if (hideSequencerBatchDirect && directSource === "SEQUENCER_BATCH") {
          return false;
        }
      }

      if (term.length === 0) {
        return true;
      }
      const haystack = JSON.stringify(event.raw).toLowerCase();
      return haystack.includes(term);
    });
  }, [events, hideSequencerBatchDirect, search, showDirect, showEnvelopeAccepted]);

  const openWindow = (windowId: string) => {
    navigate(`/monitor?windowId=${encodeURIComponent(windowId)}`);
  };

  return (
    <div className="page-grid">
      <section className="card">
        <h2>WS Feed</h2>
        <div className="row gap wrap">
          <button type="button" className="btn" onClick={connect} disabled={connected}>
            Connect
          </button>
          <button type="button" className="btn" onClick={disconnect} disabled={!connected}>
            Disconnect
          </button>
          <button type="button" className="btn btn-small" onClick={() => setEvents([])}>
            Clear events
          </button>
          <span>{connected ? "Connected" : "Disconnected"}</span>
          <span className="mono">{wsUrl}</span>
        </div>

        {connectError ? <div className="error-box">{connectError}</div> : null}

        <div className="field-grid compact">
          <label>
            <input
              type="checkbox"
              checked={showEnvelopeAccepted}
              onChange={(event) => setShowEnvelopeAccepted(event.target.checked)}
            />
            show ENVELOPE_ACCEPTED
          </label>

          <label>
            <input type="checkbox" checked={showDirect} onChange={(event) => setShowDirect(event.target.checked)} />
            show DIRECT
          </label>

          <label>
            <input
              type="checkbox"
              checked={hideSequencerBatchDirect}
              onChange={(event) => setHideSequencerBatchDirect(event.target.checked)}
            />
            hide DIRECT from SEQUENCER_BATCH
          </label>

          <label>
            Search
            <input
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="owner / grantId / windowId"
            />
          </label>
        </div>
      </section>

      <WsEventList events={filteredEvents} onOpenWindow={openWindow} />
    </div>
  );
}
