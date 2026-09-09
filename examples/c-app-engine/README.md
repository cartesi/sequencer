# C application bridge

`EngineApp` implements `sequencer_core::application::Application` using the
[application-engine C ABI](include/application-engine.h). The sequencer owns one
engine at a time. A handle can move between threads; calls on it never overlap.
The bridge is `Send`, without `Clone` or `Sync`.

The native engine owns application state and its execution count/safe-block
clock. Successful execution advances that progress, including counted no-ops;
protocol rejection leaves it unchanged. The shared Rust execution functions
check these transitions. Fatal validation, execution, or output-drain failures
return `AppError`; callers must discard that instance. Exceptions must not cross
the C ABI.

`NOT_FOUND` and `INVALID_DUMP` retain the distinction between missing/corrupt
checkpoint artifacts and other operational `IO_ERROR` failures. Error strings
are diagnostics and never determine classification. The engine and generated
bindings must use the same header, including these status declarations.

A dump prefix may be a file or a directory. Opening it produces independently
mutable state without changing the source; checkpoint creation may mutate the
engine's backing arrangement, while preserving logical state and progress.
Successful checkpoints are durable and immutable under subsequent execution.
`state_file_in_dump` names the one canonical comparison file, which can be the
whole dump or a projection alongside richer restoration artifacts. `EngineApp`
does not implement the optional Rust `CanonicalState` inspection trait.

## Reference wallet

The reference engine exports the Rust wallet through actual `extern "C"`
functions. Its static archive is also usable by the generic host. In the
repository's development shell:

```sh
cargo run -p c-wallet-engine --bin c-wallet-genesis -- /tmp/wallet-genesis devnet
cargo run -p c-wallet-sequencer -- --state-file /tmp/wallet-genesis setup
cargo run -p c-wallet-sequencer -- run
cargo test -p c-wallet-engine --test conformance
```

The ordinary setup/run environment configuration is still required. The genesis
path is required only for plain `setup`; warm startup, `flush-mempool`, and
`setup --recovery` use the sequencer's durable checkpoints.

## External engine

Build the application's static archive and use the corresponding header:

```sh
APPLICATION_ENGINE_LIB=/absolute/path/libengine.a \
APPLICATION_ENGINE_HEADER=/absolute/path/application-engine.h \
APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT=53 \
  cargo build -p c-app-sequencer
```

The payload limit must match the engine's own build. Bindgen generates the Rust
records from that header, so a build needs libclang. The engine also supplies its
own genesis tool; configuration does not cross this ABI. With no external
archive configured, the generic binary reports that no engine was linked, and
`c-wallet-sequencer` supplies the reference implementation through Cargo.

The conformance suite compares native and ABI execution over mixed inputs,
notices and vouchers, rejection/no-op progress, dump round trips, independent
instances, and fatal/error classification.
