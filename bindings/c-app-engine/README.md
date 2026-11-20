# C application bridge

The crates under `bindings/` are reusable integration libraries. The wallet
engine and binary under `examples/` demonstrate their use.

`EngineApp` implements `sequencer_core::application::Application` using the
[application-engine C ABI](include/application-engine.h). The sequencer owns one
engine at a time. A handle can move between threads; calls on it never overlap.
The bridge is `Send`, without `Clone` or `Sync`.
The [binding guide](../../docs/protocol/c-application-binding.md) maps the ABI
to the Application and scheduler contracts.

The native engine owns application state, each sender's nonce and the L2 tx
count, and holds no clock. Execution is total, the shared executor compares
nonces, admission is lane-only policy, and `EngineApp` checks the engine's ABI
version before any other call; the binding guide's
[execution and ownership](../../docs/protocol/c-application-binding.md#execution-and-ownership)
section maps each call. Fatal admission, nonce-read, execution, or
output-drain failures return `AppError`; callers must discard that instance.
Exceptions must not cross the C ABI.

`NOT_FOUND` and `INVALID_DUMP` retain the distinction between missing/corrupt
checkpoint artifacts and other operational `IO_ERROR` failures. Error strings
are diagnostics and never determine classification. The engine and generated
bindings must use the same header, including these status declarations.

A dump prefix may be a file or a directory. Opening it produces independently
mutable state without changing the source; checkpoint creation may mutate the
engine's backing arrangement, while preserving logical state and its count.
Successful checkpoints are durable and immutable under subsequent execution.
All checkpoint artifacts reside at or below the prefix; the sequencer disposes
of them with ordinary recursive filesystem deletion. Restored engines remain
usable after source deletion.
`state_file_in_dump` names the one canonical comparison file, which can be the
whole dump or a projection alongside richer restoration artifacts.
`application_engine_from_canonical_state` opens an engine over such a file at a
canonical machine's count. `EngineApp` does not implement the optional Rust
`CanonicalState` inspection trait.

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

The ordinary setup/run environment configuration is still required. Only a
setup without `--base` reads the genesis path; `setup --base` builds the
engine from the machine's state, and completed setup, warm startup, and
`flush-mempool` use the sequencer's durable checkpoints.

The host adds `--state-file` through its own parser and passes the parsed command
to `sequencer::run_command`. This shares `run_main`'s command lifecycle and exit
policy. Both take a lazy `FnOnce() -> Result<A, AppError>` genesis factory;
infallible Rust constructors therefore use
`run_main(|| Ok(WalletApp::new(WalletConfig::default())))`. An absent required
genesis path or a missing/corrupt genesis dump is a terminal bootstrap error;
operational I/O failures retain their retryable classification. A caught factory
panic follows the shared terminal-error policy.

## External engine

Build the application's static archive against the header from the chosen
sequencer revision. From a checkout of that revision, build the generic host
with that same header:

```sh
APPLICATION_ENGINE_LIB=/absolute/path/libengine.a \
APPLICATION_ENGINE_HEADER=/absolute/path/application-engine.h \
  cargo build -p c-app-sequencer
```

The linked engine reports its ABI version through
`application_engine_abi_version()`, its stable payload bound through
`application_engine_max_method_payload_bytes()`, where zero permits only empty
method payloads, and its scheduler constants (the staleness window, the fridge
bound and the direct payload cap) through
`application_engine_scheduler_limits()`. Bindgen generates the Rust records
from the header, so a build needs libclang. The engine also supplies its own genesis tool; configuration does not
cross this ABI. With no external archive configured, the generic binary reports
that no engine was linked, and
`c-wallet-sequencer` supplies the reference implementation through Cargo.

For a binary in another repository, depend on the host directly. Replace
`<full-commit-hash>` with the sequencer revision whose header the engine uses:

```toml
[dependencies]
c-app-sequencer = { git = "https://github.com/cartesi/sequencer", rev = "<full-commit-hash>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
#[tokio::main]
async fn main() -> std::process::ExitCode {
    c_app_sequencer::run().await
}
```

Set the same `APPLICATION_ENGINE_LIB` and `APPLICATION_ENGINE_HEADER` variables
when running `cargo build` in that binary's repository. The host's `run()` owns
CLI parsing and tracing setup. For custom host wiring, depend on `c-app-engine`
and `sequencer` at the same Git revision and compose `EngineApp` with
`sequencer::run_command` directly. No wallet crate is required in either case.

The conformance suite compares native and ABI execution over mixed inputs,
notices and vouchers, nonce skips, admission refusals, underfunded ops that
still execute, no-op counting, dump round trips, the canonical-state projection,
independent instances, and fatal/error classification.
`cargo test -p c-app-engine --lib` also checks mixed-output ordering, copying
reused engine buffers, full-width voucher values, the version check, and
admission's reason mapping with a small ABI fixture.

The `c_host_` scenarios in `rollups-e2e` launch the reference C host with generated
devnet genesis, delete that source before startup, and exercise ordinary execution,
clean restart, and setup's rebuild through the engine, from genesis and from a
stored canonical machine. Independent `EngineApp` replicas restore HTTP
archives, discard the downloaded sources, and follow backlog and live WS
inputs, from a snapshot to the live feed. Ordinary execution also compares the
host against the canonical machine. Run them with:

```sh
just setup
just ensure-machine-image
cargo build --locked -p c-wallet-engine --bin c-wallet-genesis -p c-wallet-sequencer --bin c-wallet-sequencer -p rollups-e2e --bin rollups-e2e
target/debug/rollups-e2e c_host_ --nocapture
```

`just test-rollups-e2e` includes these scenarios in CI. This covers the reference
wallet across the C ABI; private engines still need their own integration and
canonical comparison. No scenario starts a sequencer from a canonical machine;
the conformance suite checks `application_engine_from_canonical_state` alone.
