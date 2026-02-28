import { createContext, useContext, useMemo, useState, type ReactNode } from "react";

type TxSenderStateContextValue = {
  sessionPrivateKey: string;
  setSessionPrivateKey: (next: string) => void;
};

const TxSenderStateContext = createContext<TxSenderStateContextValue | null>(null);

export function TxSenderStateProvider({ children }: { children: ReactNode }) {
  const [sessionPrivateKey, setSessionPrivateKey] = useState("");

  const value = useMemo<TxSenderStateContextValue>(
    () => ({
      sessionPrivateKey,
      setSessionPrivateKey,
    }),
    [sessionPrivateKey],
  );

  return <TxSenderStateContext.Provider value={value}>{children}</TxSenderStateContext.Provider>;
}

export function useTxSenderState(): TxSenderStateContextValue {
  const ctx = useContext(TxSenderStateContext);
  if (!ctx) {
    throw new Error("useTxSenderState must be used within TxSenderStateProvider");
  }
  return ctx;
}
