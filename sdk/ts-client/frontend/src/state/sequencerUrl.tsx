import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from "react";

import type { FlushResponse } from "../api/types";

type SequencerSettingsContextValue = {
  sequencerBaseUrl: string;
  setSequencerBaseUrl: (next: string) => void;
  wsUrl: string;
  setWsUrl: (next: string) => void;
  autoRefresh: boolean;
  setAutoRefresh: (next: boolean) => void;
  refreshIntervalMs: number;
  setRefreshIntervalMs: (next: number) => void;
  lastFlush: { atMs: number; response: FlushResponse } | null;
  setLastFlush: (next: { atMs: number; response: FlushResponse } | null) => void;
};

const STORAGE_KEYS = {
  sequencerBaseUrl: "sequencer.console.baseUrl",
  wsUrl: "sequencer.console.wsUrl",
  autoRefresh: "sequencer.console.autoRefresh",
  refreshIntervalMs: "sequencer.console.refreshIntervalMs",
} as const;

const SequencerSettingsContext = createContext<SequencerSettingsContextValue | null>(null);

function readStringStorage(key: string, fallback: string): string {
  const value = localStorage.getItem(key);
  return value && value.trim().length > 0 ? value : fallback;
}

function readBooleanStorage(key: string, fallback: boolean): boolean {
  const value = localStorage.getItem(key);
  if (value === "true") {
    return true;
  }
  if (value === "false") {
    return false;
  }
  return fallback;
}

function readNumberStorage(key: string, fallback: number): number {
  const value = localStorage.getItem(key);
  const parsed = Number.parseInt(value ?? "", 10);
  return Number.isFinite(parsed) ? parsed : fallback;
}

export function SequencerSettingsProvider({ children }: { children: ReactNode }) {
  const [sequencerBaseUrl, setSequencerBaseUrl] = useState<string>(() =>
    readStringStorage(STORAGE_KEYS.sequencerBaseUrl, "http://localhost:18080"),
  );
  const [wsUrl, setWsUrl] = useState<string>(() => readStringStorage(STORAGE_KEYS.wsUrl, "ws://localhost:8081"));
  const [autoRefresh, setAutoRefresh] = useState<boolean>(() => readBooleanStorage(STORAGE_KEYS.autoRefresh, true));
  const [refreshIntervalMs, setRefreshIntervalMs] = useState<number>(() =>
    readNumberStorage(STORAGE_KEYS.refreshIntervalMs, 2000),
  );
  const [lastFlush, setLastFlush] = useState<{ atMs: number; response: FlushResponse } | null>(null);

  useEffect(() => {
    localStorage.setItem(STORAGE_KEYS.sequencerBaseUrl, sequencerBaseUrl);
  }, [sequencerBaseUrl]);

  useEffect(() => {
    localStorage.setItem(STORAGE_KEYS.wsUrl, wsUrl);
  }, [wsUrl]);

  useEffect(() => {
    localStorage.setItem(STORAGE_KEYS.autoRefresh, autoRefresh ? "true" : "false");
  }, [autoRefresh]);

  useEffect(() => {
    localStorage.setItem(STORAGE_KEYS.refreshIntervalMs, String(refreshIntervalMs));
  }, [refreshIntervalMs]);

  const value = useMemo<SequencerSettingsContextValue>(
    () => ({
      sequencerBaseUrl,
      setSequencerBaseUrl,
      wsUrl,
      setWsUrl,
      autoRefresh,
      setAutoRefresh,
      refreshIntervalMs,
      setRefreshIntervalMs,
      lastFlush,
      setLastFlush,
    }),
    [sequencerBaseUrl, wsUrl, autoRefresh, refreshIntervalMs, lastFlush],
  );

  return <SequencerSettingsContext.Provider value={value}>{children}</SequencerSettingsContext.Provider>;
}

export function useSequencerSettings(): SequencerSettingsContextValue {
  const ctx = useContext(SequencerSettingsContext);
  if (!ctx) {
    throw new Error("useSequencerSettings must be used within SequencerSettingsProvider");
  }
  return ctx;
}
