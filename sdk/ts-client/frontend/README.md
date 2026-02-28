# Sequencer Console

Minimal React + Vite + TypeScript console for the standalone sequencer.

## Required Sequencer Endpoints

This UI only uses existing sequencer APIs:

- `GET /v0/status`
- `GET /v0/snapshot`
- `POST /v0/register_grant`
- `POST /v0/envelopes`
- `POST /v0/flush`
- `GET /v0/batch/active`
- `GET /v0/batch/pending`
- `GET /v0/batch/{windowId}`
- WebSocket feed (`exec.input` events)

No backend endpoint changes are required.

## Defaults

- Sequencer base URL: `http://localhost:18080`
- WS URL: `ws://localhost:8081`

Both are editable in the top bar and persisted in localStorage.

## Run

```bash
cd frontend
bun install
bun run dev
```

If you prefer pnpm script conventions:

```bash
pnpm dev
```

Build:

```bash
bun run build
```

## Pages

- **Monitor**
  - Polls status/snapshot/active/pending.
  - Default introspection is redacted (`includeBytes=false`, `includeSigs=false`).
  - Supports debug query toggles and pending-window drilldown via `/v0/batch/{windowId}`.
- **Tx Sender**
  - Owner signing via wagmi injected wallet.
  - Session signing via in-memory private key.
  - Grant flow: sign `SessionKeyGrant`, compute `grantId`, register grant.
  - Envelope flow: build action bytes (place/cancel/withdraw), sign typed data, submit to sequencer.
- **WS Feed**
  - Connect/disconnect to sequencer WS.
  - Filter ENVELOPE_ACCEPTED/DIRECT, hide sequencer batch DIRECT, text search, inspect raw JSON.
