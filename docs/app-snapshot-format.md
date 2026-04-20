# App Snapshot Format (Wallet Toy App)

This document defines the current on-disk snapshot format for the toy wallet app in `examples/app-core`.

Scope note: this only covers the **capability** to serialize/deserialize app state. It does not define when snapshots are triggered or how runtime wiring invokes save/load.

## Encoding

- **Format:** SSZ
- **Current version:** `WalletSnapshotV1` (Rust struct name)
- **Byte order for balances:** big-endian 32-byte integers (`U256`)

## Serialized State

`WalletSnapshotV1` encodes:

- `erc20_portal_address` (`[u8; 20]`)
- `supported_erc20_token` (`[u8; 20]`)
- `sequencer_address` (`[u8; 20]`)
- `balances` (`Vec<SnapshotBalance>`)
  - `address` (`[u8; 20]`)
  - `balance_be` (`[u8; 32]`)
- `nonces` (`Vec<SnapshotNonce>`)
  - `address` (`[u8; 20]`)
  - `nonce` (`u32`)
- `executed_input_count` (`u64`)

## Determinism Guarantees

`WalletApp` stores balances/nonces in hash maps, so iteration order is nondeterministic. Before encoding:

- `balances` entries are sorted by `address`
- `nonces` entries are sorted by `address`

This guarantees stable snapshot bytes for equivalent logical state.

## Compatibility Policy

- Restores must decode the exact current snapshot schema.
- Malformed bytes fail restore with a decode error.
- Future breaking changes must introduce a new versioned schema type (for example, `WalletSnapshotV2`) and explicit migration/dispatch logic.
- Do not reorder or reinterpret existing fields in-place without a version bump.

## API Surface

Current wallet snapshot API:

- `snapshot_bytes(&self) -> Vec<u8>`
- `restore_from_snapshot_bytes(&mut self, snapshot: &[u8]) -> Result<(), WalletSnapshotError>`
- `save_snapshot<P: AsRef<Path>>(&self, path: P) -> Result<(), WalletSnapshotError>`
- `load_snapshot<P: AsRef<Path>>(&mut self, path: P) -> Result<(), WalletSnapshotError>`

## Disk Write Semantics

`save_snapshot` uses an atomic replacement pattern:

- write bytes to a temporary file in the same directory as the target
- `sync_all` the temp file
- rename temp file to the final path

This avoids exposing partially written snapshot bytes at the target path.

## Out of Scope

This document intentionally does not define:

- periodic vs explicit snapshot trigger policy
- mount paths and runtime drive conventions
- atomic file replacement protocol for production snapshot lifecycle
