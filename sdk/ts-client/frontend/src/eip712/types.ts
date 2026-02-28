export const EIP712_DOMAIN_NAME = "CartesiDex";
export const EIP712_DOMAIN_VERSION = "1";

export const ZERO_GRANT_ID =
  "0x0000000000000000000000000000000000000000000000000000000000000000" as const;

export const SESSION_SCOPE_TRADE = 1n;
export const SESSION_SCOPE_CANCEL = 2n;
export const SESSION_SCOPE_WITHDRAW = 4n;

export const SESSION_KEY_GRANT_TYPES = {
  SessionKeyGrant: [
    { name: "owner", type: "address" },
    { name: "sessionKey", type: "address" },
    { name: "validUntilTickId", type: "uint256" },
    { name: "scopes", type: "uint256" },
  ],
} as const;

export const TRADING_ENVELOPE_TYPES = {
  TradingEnvelope: [
    { name: "owner", type: "address" },
    { name: "nonce", type: "uint256" },
    { name: "minInputIndexPlusOne", type: "uint256" },
    { name: "validUntilTickId", type: "uint256" },
    { name: "actionHash", type: "bytes32" },
    { name: "grantId", type: "bytes32" },
  ],
} as const;

export type SessionKeyGrantMessage = {
  owner: `0x${string}`;
  sessionKey: `0x${string}`;
  validUntilTickId: bigint;
  scopes: bigint;
};

export type TradingEnvelopeMessage = {
  owner: `0x${string}`;
  nonce: bigint;
  minInputIndexPlusOne: bigint;
  validUntilTickId: bigint;
  actionHash: `0x${string}`;
  grantId: `0x${string}`;
};
