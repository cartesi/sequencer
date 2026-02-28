export type ApiErrorResponse = { error: string };

export type StatusResponse = {
  l1Timestamp: string;
  windowId: string;
  windowEndTs: string;
  safeProcessedCount: string;
  safeCount: string;
  safeHead: string;
  emittedNextIndex: string;
  readerLag: string;
  currentBufferSize: number;
  pendingWindowCount: number;
  pendingEnvelopeCount: number;
  pendingWindows: Array<{
    windowId: string;
    count: number;
    totalEnvelopeBytes: number;
  }>;
  lifecyclePostedCount: number;
  lifecycleFailedCount: number;
  lifecycleAckedCount: number;
};

export type SnapshotResponse = {
  chainId: number;
  app: `0x${string}`;
  minInputIndexPlusOne: string;
  windowId: string;
  windowEndTs: string;
};

export type SessionKeyGrant = {
  owner: `0x${string}`;
  sessionKey: `0x${string}`;
  validUntilTickId: string;
  scopes: string;
};

export type SessionGrantBundle = {
  grant: SessionKeyGrant;
  ownerSig: `0x${string}`;
  grantId: `0x${string}`;
};

export type RegisterGrantRequest = {
  grant: SessionKeyGrant;
  ownerSig: `0x${string}`;
};

export type RegisterGrantResponse = {
  ok: true;
  grantId: `0x${string}`;
};

export type SubmitEnvelopeRequest = {
  envelope: {
    owner: `0x${string}`;
    nonce: string;
    minInputIndexPlusOne: string;
    validUntilTickId: string;
    grantId: `0x${string}`;
    actionBytes: `0x${string}`;
  };
  ownerSig?: `0x${string}`;
  sessionSig?: `0x${string}`;
};

export type SubmitEnvelopeResponse = {
  accepted: true;
  windowId: string;
  seq: number;
  minInputIndexPlusOne: string;
  envelopeHash: `0x${string}`;
  actionBytes: `0x${string}`;
  authMode: "OWNER_SIG" | "SESSION_SIG";
  signerRecovered: `0x${string}`;
  grantId?: `0x${string}`;
};

export type FlushResponse =
  | {
      flushed: false;
      windowId: string;
      count: number;
    }
  | {
      flushed: true;
      windowId: string;
      count: number;
      txHash: `0x${string}`;
      status: "POSTED";
      payloadHash?: `0x${string}`;
    };

export type WindowGrantSummary =
  | {
      grantId: `0x${string}`;
      owner: `0x${string}`;
      sessionKey: `0x${string}`;
      validUntilTickId: string;
      scopes: string;
      ownerSig?: `0x${string}`;
      missing?: never;
    }
  | {
      grantId: `0x${string}`;
      missing: true;
      owner?: never;
      sessionKey?: never;
      validUntilTickId?: never;
      scopes?: never;
      ownerSig?: never;
    };

export type WindowEnvelopeSummary = {
  seq: number;
  envelopeHash: `0x${string}`;
  owner: `0x${string}`;
  authMode: "OWNER_SIG" | "SESSION_SIG";
  grantId: `0x${string}` | null;
  nonce: string;
  minInputIndexPlusOne: string;
  validUntilTickId: string;
  actionType: number | null;
  actionBytesLen: number;
  actionBytes?: `0x${string}`;
  acceptedEnvelopeBytes?: `0x${string}`;
  signature?: `0x${string}`;
};

export type WindowIntrospection = {
  windowId: string;
  state: "ACTIVE" | "BUFFERED" | "POSTED" | "ACKED" | "FAILED" | "EXPIRED";
  openedAtMs: string | null;
  ageMs: string | null;
  limits: {
    maxAgeMs: number;
    maxCount: number;
    maxBytes: number;
  };
  count: number;
  bytes: number;
  grants: WindowGrantSummary[];
  envelopes: WindowEnvelopeSummary[];
  txHash?: `0x${string}` | null;
  postedAtMs?: string | null;
};

export type PendingWindowSummary = {
  windowId: string;
  state: "BUFFERED" | "POSTED" | "ACKED" | "FAILED" | "EXPIRED";
  count: number;
  bytes: number;
  grantCount: number;
  txHash?: `0x${string}`;
  postedAtMs?: string;
};

export type PendingWindowsSummaryResponse = {
  windows: PendingWindowSummary[];
};

export type PendingWindowsDetailResponse = {
  windows: WindowIntrospection[];
};
