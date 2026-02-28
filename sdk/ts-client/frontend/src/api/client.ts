import type {
  ApiErrorResponse,
  FlushResponse,
  PendingWindowsDetailResponse,
  PendingWindowsSummaryResponse,
  RegisterGrantRequest,
  RegisterGrantResponse,
  SnapshotResponse,
  StatusResponse,
  SubmitEnvelopeRequest,
  SubmitEnvelopeResponse,
  WindowIntrospection,
} from "./types";
import { tryParseJson } from "../utils/json";

export type IntrospectionQuery = {
  includeBytes?: boolean;
  includeSigs?: boolean;
  limitEnvelopes?: number;
};

export type PendingQuery = IntrospectionQuery & {
  includeDetails?: boolean;
};

function buildUrl(baseUrl: string, path: string, query?: Record<string, string | number | boolean | undefined>): string {
  const base = baseUrl.endsWith("/") ? baseUrl.slice(0, -1) : baseUrl;
  const url = new URL(`${base}${path}`);
  if (query) {
    for (const [key, value] of Object.entries(query)) {
      if (value === undefined) {
        continue;
      }
      url.searchParams.set(key, String(value));
    }
  }
  return url.toString();
}

export class SequencerApiError extends Error {
  public readonly status: number;
  public readonly payload: unknown;

  public constructor(status: number, message: string, payload: unknown) {
    super(message);
    this.name = "SequencerApiError";
    this.status = status;
    this.payload = payload;
  }
}

async function requestJson<T>(url: string, init?: RequestInit): Promise<T> {
  const headers: Record<string, string> = {
    ...(init?.headers ? (init.headers as Record<string, string>) : {}),
  };
  if (init?.body !== undefined && headers["content-type"] === undefined) {
    headers["content-type"] = "application/json";
  }

  const response = await fetch(url, {
    ...init,
    headers,
  });

  const text = await response.text();
  const parsed = text.length > 0 ? tryParseJson(text) : null;

  if (!response.ok) {
    const payload = parsed as ApiErrorResponse | null;
    const message = payload?.error ?? `${response.status} ${response.statusText}`;
    throw new SequencerApiError(response.status, message, parsed);
  }

  return (parsed as T) ?? ({} as T);
}

export const apiClient = {
  getStatus(baseUrl: string): Promise<StatusResponse> {
    return requestJson<StatusResponse>(buildUrl(baseUrl, "/v0/status"));
  },

  getSnapshot(baseUrl: string): Promise<SnapshotResponse> {
    return requestJson<SnapshotResponse>(buildUrl(baseUrl, "/v0/snapshot"));
  },

  registerGrant(baseUrl: string, payload: RegisterGrantRequest): Promise<RegisterGrantResponse> {
    return requestJson<RegisterGrantResponse>(buildUrl(baseUrl, "/v0/register_grant"), {
      method: "POST",
      body: JSON.stringify(payload),
    });
  },

  submitEnvelope(baseUrl: string, payload: SubmitEnvelopeRequest): Promise<SubmitEnvelopeResponse> {
    return requestJson<SubmitEnvelopeResponse>(buildUrl(baseUrl, "/v0/envelopes"), {
      method: "POST",
      body: JSON.stringify(payload),
    });
  },

  flushOldest(baseUrl: string): Promise<FlushResponse> {
    return requestJson<FlushResponse>(buildUrl(baseUrl, "/v0/flush"), {
      method: "POST",
      body: JSON.stringify({}),
    });
  },

  getActiveWindow(baseUrl: string, query: IntrospectionQuery): Promise<WindowIntrospection> {
    return requestJson<WindowIntrospection>(
      buildUrl(baseUrl, "/v0/batch/active", {
        includeBytes: query.includeBytes,
        includeSigs: query.includeSigs,
        limitEnvelopes: query.limitEnvelopes,
      }),
    );
  },

  getPendingWindows(
    baseUrl: string,
    query: PendingQuery,
  ): Promise<PendingWindowsSummaryResponse | PendingWindowsDetailResponse> {
    return requestJson<PendingWindowsSummaryResponse | PendingWindowsDetailResponse>(
      buildUrl(baseUrl, "/v0/batch/pending", {
        includeDetails: query.includeDetails,
        includeBytes: query.includeBytes,
        includeSigs: query.includeSigs,
        limitEnvelopes: query.limitEnvelopes,
      }),
    );
  },

  getWindowById(baseUrl: string, windowId: string, query: IntrospectionQuery): Promise<WindowIntrospection> {
    return requestJson<WindowIntrospection>(
      buildUrl(baseUrl, `/v0/batch/${windowId}`, {
        includeBytes: query.includeBytes,
        includeSigs: query.includeSigs,
        limitEnvelopes: query.limitEnvelopes,
      }),
    );
  },
};
