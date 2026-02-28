import { z } from "zod";

import { parseNumberValue } from "./utils.js";

export type SequencerConfig = {
  chainId: number;
  l1RpcUrl: string;
  rollupsRpcUrl: string;
  appAddress: `0x${string}`;
  inputBoxAddress: `0x${string}`;
  sequencerPrivateKey: `0x${string}`;
  sequencerAddress: `0x${string}`;
  batchMaxAgeMs: number;
  ackTimeoutMs: number;
  maxEnvelopeBytes: number;
  maxBatchBytes: number;
  maxBatchCount: number;
  pollMs: number;
  httpPort: number;
  wsPort: number;
  wsReplayCount: number;
  stateFilePath: string;
  batcherJournalPath: string;
  eip712Name: string;
  eip712Version: string;
  logLevel: "fatal" | "error" | "warn" | "info" | "debug" | "trace" | "silent";
  directEmitLogEvery: number;
};

const envSchema = z.object({
  CHAIN_ID: z.string(),
  L1_RPC_URL: z.string().url(),
  ROLLUPS_RPC_URL: z.string().url(),
  APP_ADDRESS: z.string().regex(/^0x[a-fA-F0-9]{40}$/),
  INPUT_BOX_ADDRESS: z.string().regex(/^0x[a-fA-F0-9]{40}$/),
  SEQUENCER_PRIVATE_KEY: z.string().regex(/^0x[a-fA-F0-9]{64}$/),
  SEQUENCER_ADDRESS: z.string().regex(/^0x[a-fA-F0-9]{40}$/),
  BATCH_MAX_AGE_MS: z.string().optional(),
  ACK_TIMEOUT_MS: z.string().optional(),
  MAX_ENVELOPE_BYTES: z.string().optional(),
  MAX_BATCH_BYTES: z.string().optional(),
  MAX_BATCH_COUNT: z.string().optional(),
  POLL_MS: z.string().optional(),
  HTTP_PORT: z.string().optional(),
  WS_PORT: z.string().optional(),
  WS_REPLAY_COUNT: z.string().optional(),
  STATE_FILE_PATH: z.string().optional(),
  BATCHER_JOURNAL_PATH: z.string().optional(),
  EIP712_NAME: z.string().optional(),
  EIP712_VERSION: z.string().optional(),
  LOG_LEVEL: z.enum(["fatal", "error", "warn", "info", "debug", "trace", "silent"]).optional(),
  DIRECT_EMIT_LOG_EVERY: z.string().optional(),
});

export function loadConfig(env: NodeJS.ProcessEnv = process.env): SequencerConfig {
  const parsed = envSchema.safeParse(env);
  if (!parsed.success) {
    const issues = parsed.error.issues.map((issue) => issue.path.join(".")).join(", ");
    throw new Error(`Missing/invalid env config: ${issues}`);
  }

  const values = parsed.data;
  const appAddress = values.APP_ADDRESS as `0x${string}`;
  const inputBoxAddress = values.INPUT_BOX_ADDRESS as `0x${string}`;
  const sequencerPrivateKey = values.SEQUENCER_PRIVATE_KEY as `0x${string}`;
  const sequencerAddress = values.SEQUENCER_ADDRESS as `0x${string}`;
  const stateFilePath = values.STATE_FILE_PATH ?? "./sequencer_state.json";
  const batchMaxAgeMs = parseNumberValue(values.BATCH_MAX_AGE_MS ?? "10000", "BATCH_MAX_AGE_MS");
  const ackTimeoutMs = parseNumberValue(values.ACK_TIMEOUT_MS ?? "30000", "ACK_TIMEOUT_MS");

  return {
    chainId: parseNumberValue(values.CHAIN_ID, "CHAIN_ID"),
    l1RpcUrl: values.L1_RPC_URL,
    rollupsRpcUrl: values.ROLLUPS_RPC_URL,
    appAddress,
    inputBoxAddress,
    sequencerPrivateKey,
    sequencerAddress,
    batchMaxAgeMs,
    ackTimeoutMs,
    maxEnvelopeBytes: parseNumberValue(values.MAX_ENVELOPE_BYTES ?? "8192", "MAX_ENVELOPE_BYTES"),
    maxBatchBytes: parseNumberValue(values.MAX_BATCH_BYTES ?? "1000000", "MAX_BATCH_BYTES"),
    maxBatchCount: parseNumberValue(values.MAX_BATCH_COUNT ?? "10000", "MAX_BATCH_COUNT"),
    pollMs: parseNumberValue(values.POLL_MS ?? "200", "POLL_MS"),
    httpPort: parseNumberValue(values.HTTP_PORT ?? "18080", "HTTP_PORT"),
    wsPort: parseNumberValue(values.WS_PORT ?? "8081", "WS_PORT"),
    wsReplayCount: Math.max(0, parseNumberValue(values.WS_REPLAY_COUNT ?? "256", "WS_REPLAY_COUNT")),
    stateFilePath,
    batcherJournalPath: values.BATCHER_JOURNAL_PATH ?? `${stateFilePath}.batcher_journal.ndjson`,
    eip712Name: values.EIP712_NAME ?? "CartesiDexSequencer",
    eip712Version: values.EIP712_VERSION ?? "0",
    logLevel: values.LOG_LEVEL ?? "info",
    directEmitLogEvery: Math.max(
      1,
      parseNumberValue(values.DIRECT_EMIT_LOG_EVERY ?? "100", "DIRECT_EMIT_LOG_EVERY"),
    ),
  };
}
