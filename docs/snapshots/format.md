# Wallet snapshot format

This document owns the wallet's application-state bytes, implemented by
[`wallet_snapshot.rs`](../../examples/app-core/src/wallet_snapshot.rs).
The [Application contract](../protocol/application-contract.md#6-checkpoint-lifecycle)
owns the dump methods, durability, and independent restoration;
[lifecycle.md](lifecycle.md) owns the enclosing artifact, metadata, selection,
and retention.

## Toy Wallet Layout

`WalletApp::state_file_in_dump(prefix)` returns `prefix/state`. The dump
contains exactly that one file. The wallet's persistence representation
and its canonical state coincide; one write per `create_dump`.

```
{prefix}/
  state    SSZ-encoded WalletSnapshot bytes
```

Here `prefix` is the app-owned `state` directory inside the sequencer's dump
directory. Thus the wallet file is `dumps/<id>/state/state`. The surrounding
`info.toml` and exported `checkpoint.toml` use the sequencer's metadata format
version; that version does not identify the wallet's SSZ schema. Application
progress is embedded in the wallet bytes. History identity and acceptance
metadata come from the sequencer; see [application history](../protocol/application-history.md).

## Toy Wallet Wire Format

- **Encoding**: SSZ
- **Top-level type**: `WalletSnapshot`
- **Byte order for balances**: big-endian 32-byte integers (`U256`)

### Schema

`WalletSnapshot`:

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
- `last_executed_safe_block` (`u64`) — the app's safe-block clock
  (reported by `Application::progress`): max block carried by any
  executed input. Recovery reads it as `A`, the safe block this state
  reflects, so it must live in the canonical state bytes (both the
  bare-metal and canonical-machine sides advance it identically).

### Determinism

`WalletApp` stores balances and nonces in `HashMap`s, so iteration order
is nondeterministic. Before encoding:

- `balances` entries are sorted ascending by `address`
- `nonces` entries are sorted ascending by `address`

This guarantees byte-identical snapshot files for byte-identical logical
state, regardless of insertion order. Tests assert this property by
calling `create_dump` twice on the same `WalletApp` and comparing the
resulting state files for byte equality.

### Decode Rules

The decoder rejects:

- Malformed SSZ bytes (any decode error from the SSZ library).
- A snapshot containing two entries in `balances` with the same address.
- A snapshot containing two entries in `nonces` with the same address.
- Zero `executed_input_count` with a nonzero `last_executed_safe_block`.

Duplicate checks prevent an entry from silently overwriting another during
restore. The decoder accepts unique entries in any order; encoding the restored
state sorts them. Deterministic emitted bytes do not imply that the decoder
accepts only that ordering.

## Versioning

There is one SSZ schema, `WalletSnapshot`, with no leading version tag.

If a future change ever needs to break the wire format against live dumps:

1. Introduce a new, explicitly versioned schema type (e.g. `WalletSnapshotV2`).
2. Provide explicit dispatch at the protocol layer — an HTTP route prefix
   (`/state/v2/...`), a `Content-Type` header, or whatever the consumer and
   sequencer agree on — so consumers know which decoder to use; the bytes
   themselves stay tag-less.

Until then, do not reorder, repurpose, or reinterpret existing fields in place.

## Trust Model

The file shares the persistent data directory's
[trust boundary](../threat-model/README.md). Its bytes contain no integrity tag,
checksum, or HMAC. Distribution through a less trusted channel would require
an outer integrity mechanism.
