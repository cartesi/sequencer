import { useState } from "react";
import { generatePrivateKey } from "viem/accounts";

import { useDerivedSessionAccount } from "../hooks/useDerivedSessionAccount";
import { CopyButton } from "./CopyButton";

type Props = {
  sessionPrivateKey: string;
  onChangePrivateKey: (next: string) => void;
};

export function SessionKeyPanel({ sessionPrivateKey, onChangePrivateKey }: Props) {
  const [showPrivateKey, setShowPrivateKey] = useState(false);
  const sessionAccount = useDerivedSessionAccount(sessionPrivateKey);

  return (
    <section className="card">
      <h3>Session Key (in-memory only)</h3>
      <div className="row gap">
        <button type="button" className="btn btn-small" onClick={() => onChangePrivateKey(generatePrivateKey())}>
          Generate session key
        </button>
        <button type="button" className="btn btn-small" onClick={() => setShowPrivateKey((v) => !v)}>
          {showPrivateKey ? "Hide" : "Show"} key
        </button>
      </div>

      <label>
        Session private key
        <input
          type={showPrivateKey ? "text" : "password"}
          value={sessionPrivateKey}
          onChange={(event) => onChangePrivateKey(event.target.value)}
          placeholder="0x..."
        />
      </label>

      <div>
        <div className="muted">Session address</div>
        <div className="mono inline-with-btn">
          {sessionAccount ? sessionAccount.address : "Invalid or missing private key"}
          {sessionAccount ? <CopyButton value={sessionAccount.address} /> : null}
        </div>
      </div>
    </section>
  );
}
