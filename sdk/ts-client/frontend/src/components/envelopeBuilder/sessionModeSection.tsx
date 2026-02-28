import type { SessionReadiness, GrantBundleStatus } from "./sessionValidation";

type Props = {
  grantBundleStatus: GrantBundleStatus;
  ensureGrantRegistered: boolean;
  canPreRegisterGrant: boolean;
  sessionReadiness: SessionReadiness;
  onToggleEnsureGrantRegistered: (next: boolean) => void;
};

export function SessionModeSection({
  grantBundleStatus,
  ensureGrantRegistered,
  canPreRegisterGrant,
  sessionReadiness,
  onToggleEnsureGrantRegistered,
}: Props) {
  return (
    <div>
      <div className="row gap wrap">
        <span className="muted">Grant bundle status</span>
        <span className={`badge ${grantBundleStatus?.tone === "ok" ? "badge-ok" : "badge-warn"}`}>
          {grantBundleStatus?.label ?? "UNKNOWN"}
        </span>
      </div>
      <div className="muted">{grantBundleStatus?.detail ?? "Unknown grant bundle state."}</div>
      <label className="inline-check">
        <input
          type="checkbox"
          checked={ensureGrantRegistered}
          onChange={(event) => onToggleEnsureGrantRegistered(event.target.checked)}
        />
        Ensure grant is registered before submit (idempotent)
      </label>
      {ensureGrantRegistered ? (
        <div className="muted">
          {canPreRegisterGrant
            ? "Submit will pre-register this grant before posting envelope."
            : "Submit will skip pre-registration because signed bundle does not match current session context."}
        </div>
      ) : null}
      {!sessionReadiness.ready ? (
        <div className="error-box">
          Session readiness:
          <ul>
            {sessionReadiness.reasons.map((reason) => (
              <li key={reason}>{reason}</li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}
