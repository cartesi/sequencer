# App Snapshot Format (Wallet Toy App)

This document defines the on-disk snapshot format produced by the toy wallet
app in `examples/app-core` via the `Application` trait's dump methods.

## Scope

This document covers two things:

1. The trait shape that any `Application` implementation must satisfy to
   participate in snapshot lifecycle (`from_dump`, `create_dump`,
   `delete_dump`, `state_file_in_dump`).
2. The wire format the toy wallet uses to encode its canonical state into
   the dump's state file.

It does NOT define when snapshots are triggered, how the inclusion lane
records and promotes them, how the HTTP layer serves them, or recovery
interactions. Those are layered above the trait and live in their own
modules.

## Trait Surface

```rust
trait Application: Send + Sized {
    // ... other methods ...

    fn from_dump(prefix: &Path) -> Result<Self, AppError>;
    fn create_dump(&self, prefix: &Path) -> Result<(), AppError>;
    fn delete_dump(prefix: &Path) -> Result<(), AppError>;
    fn state_file_in_dump(prefix: &Path) -> PathBuf;
}
```

Contract:

- `prefix` is always a directory. The impl owns whatever layout lives
  inside; callers treat the path as opaque after creation.
- `create_dump` is responsible for creating `prefix` (which must not
  already exist) and writing the dump artifacts inside.
- `state_file_in_dump` is a pure function of `prefix`: callers may
  compute it without loading the dump or instantiating the Application.
  Each impl pins its own layout convention.
- The bytes at `state_file_in_dump(prefix)` are the canonical state —
  the bytes a watchdog running an independent canonical machine would
  produce for the same logical state via its `inspect_state` procedure.
  They must be deterministic: identical logical state must produce
  byte-identical files across runs, hosts, and toolchains.
- For implementations whose persistence representation IS the canonical
  state (the toy wallet, the bare-metal DEX), `create_dump` writes the
  same bytes that `state_file_in_dump` names — a single file with no
  duplication. For implementations whose persistence is richer than the
  canonical state (e.g. a Cartesi Machine wrapping app), `create_dump`
  writes the full machine state alongside a separate canonical-state file
  under the same prefix.

Genesis construction is intentionally not on the trait. The way an
Application comes into existence at cold start varies per impl (CLI
config for the toy wallet, machine image path for a CM-wrapping app,
etc.) and lives on the concrete type, called by the runtime at bootstrap.
The inclusion lane only needs `from_dump` to rehydrate from a previously
persisted state during catch-up.

## Toy Wallet Layout

`WalletApp::state_file_in_dump(prefix)` returns `prefix/state`. The dump
contains exactly that one file. The wallet's persistence representation
and its canonical state coincide; one write per `create_dump`.

```
{prefix}/
  state    SSZ-encoded WalletSnapshot bytes
```

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
  (`Application::last_executed_safe_block`): max block carried by any
  executed input. Recovery reads it as `A`, the safe block this state
  reflects, so it must live in the canonical state bytes (both the
  bare-metal and canonical-machine sides advance it identically).

`last_executed_safe_block` was added before any environment was deployed; there
is a single, unversioned schema today (see [Versioning](#versioning)).

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

The duplicate-address checks exist to keep the encoded bytes canonical:
without them, multiple distinct byte sequences could decode to the same
logical state (the second entry would silently overwrite the first),
breaking the property that watchdog-side and sequencer-side bytes are
comparable.

## Versioning

There is a single, unversioned schema: `WalletSnapshot`. The encoded bytes carry
no leading version tag, and — because there is no backward-compatibility
requirement yet (no long-lived deployment whose dumps a newer binary must read) —
the struct name carries no version suffix either. An earlier draft distinguished
a `V1`/`V2` pair (the `last_executed_safe_block` field was added before any
environment existed); that split was collapsed since no `V1` dumps ever survived.

If a future change ever needs to break the wire format against live dumps:

1. Introduce a new, explicitly versioned schema type (e.g. `WalletSnapshotV2`).
2. Provide explicit dispatch at the protocol layer — an HTTP route prefix
   (`/state/v2/...`), a `Content-Type` header, or whatever the consumer and
   sequencer agree on — so consumers know which decoder to use; the bytes
   themselves stay tag-less.

Until then, do not reorder, repurpose, or reinterpret existing fields in place.

## Trust Model

The dump file is part of the sequencer's persistent data directory and
shares its trust boundary. An attacker with write access to the data
directory has already won; no integrity tag, checksum, or HMAC is
included on the snapshot bytes for this reason. Consumers that obtain
the bytes via a less trusted channel (e.g. a future peer-to-peer
distribution mechanism) would need to add an outer integrity layer; the
format itself does not provide one.

## Out of Scope

This document deliberately does not define:

- When the inclusion lane decides to take a snapshot.
- How dumps are registered, promoted from pending to finalized, or
  garbage-collected.
- The on-the-wire archive format for streaming a dump over HTTP.
- Inspect-state procedures on other implementations (Cartesi Machine,
  bare-metal DEX).
- Cross-implementation determinism test vectors (will land when a
  second implementation of the wallet exists to validate against).
