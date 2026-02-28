import type { WindowIntrospection } from "../api/types";
import { CopyButton } from "./CopyButton";

export function WindowDetail({ title, window }: { title: string; window: WindowIntrospection | null }) {
  if (!window) {
    return (
      <section className="card">
        <h3>{title}</h3>
        <div className="muted">No window selected</div>
      </section>
    );
  }

  return (
    <section className="card">
      <h3>{title}</h3>
      <div className="kpi-grid">
        <div>
          <span className="muted">windowId</span>
          <div className="mono">{window.windowId}</div>
        </div>
        <div>
          <span className="muted">state</span>
          <div>{window.state}</div>
        </div>
        <div>
          <span className="muted">ageMs</span>
          <div>{window.ageMs ?? "-"}</div>
        </div>
        <div>
          <span className="muted">count / bytes</span>
          <div>
            {window.count} / {window.bytes}
          </div>
        </div>
      </div>

      <h4>Grants ({window.grants.length})</h4>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>grantId</th>
              <th>owner</th>
              <th>sessionKey</th>
              <th>scopes</th>
              <th>validUntilTickId</th>
              <th>sig</th>
            </tr>
          </thead>
          <tbody>
            {window.grants.length === 0 ? (
              <tr>
                <td colSpan={6} className="muted">
                  no grants
                </td>
              </tr>
            ) : (
              window.grants.map((grant) => (
                <tr key={grant.grantId}>
                  <td className="mono inline-with-btn">
                    {grant.grantId} <CopyButton value={grant.grantId} />
                  </td>
                  <td className="mono inline-with-btn">
                    {"owner" in grant ? grant.owner : grant.missing ? "missing" : "-"}
                    {"owner" in grant && grant.owner ? <CopyButton value={grant.owner} /> : null}
                  </td>
                  <td className="mono inline-with-btn">
                    {"sessionKey" in grant ? grant.sessionKey : "-"}
                    {"sessionKey" in grant && grant.sessionKey ? <CopyButton value={grant.sessionKey} /> : null}
                  </td>
                  <td>{"scopes" in grant ? grant.scopes : "-"}</td>
                  <td>{"validUntilTickId" in grant ? grant.validUntilTickId : "-"}</td>
                  <td className="mono">{"ownerSig" in grant && grant.ownerSig ? `${grant.ownerSig.slice(0, 12)}...` : "-"}</td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>

      <h4>Envelopes ({window.envelopes.length})</h4>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>seq</th>
              <th>owner</th>
              <th>mode</th>
              <th>grantId</th>
              <th>nonce</th>
              <th>minInput+1</th>
              <th>validUntilTickId</th>
              <th>actionType</th>
              <th>actionBytesLen</th>
              <th>actionBytes</th>
            </tr>
          </thead>
          <tbody>
            {window.envelopes.length === 0 ? (
              <tr>
                <td colSpan={10} className="muted">
                  no envelopes
                </td>
              </tr>
            ) : (
              window.envelopes.map((envelope) => (
                <tr key={`${envelope.seq}-${envelope.envelopeHash}`}>
                  <td>{envelope.seq}</td>
                  <td className="mono inline-with-btn">
                    {envelope.owner}
                    <CopyButton value={envelope.owner} />
                  </td>
                  <td>{envelope.authMode}</td>
                  <td className="mono inline-with-btn">
                    {envelope.grantId ?? "-"}
                    {envelope.grantId ? <CopyButton value={envelope.grantId} /> : null}
                  </td>
                  <td>{envelope.nonce}</td>
                  <td>{envelope.minInputIndexPlusOne}</td>
                  <td>{envelope.validUntilTickId}</td>
                  <td>{envelope.actionType ?? "-"}</td>
                  <td>{envelope.actionBytesLen}</td>
                  <td className="mono inline-with-btn">
                    {envelope.actionBytes ?? "[redacted]"}
                    {envelope.actionBytes ? <CopyButton value={envelope.actionBytes} /> : null}
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
