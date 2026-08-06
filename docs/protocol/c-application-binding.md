# C Application Binding

How an application that is not written in Rust is sequenced.

The sequencer reaches an application through the `Application` trait, specified in
[`application-contract.md`](application-contract.md). That document is binding here too: this one
only describes how the same contract is spelled in C, and what a C engine owes that a Rust one
gets from the type system.

The surface is one header, `examples/c-app-engine/include/application-engine.h`. An application
implements it into a static archive. `examples/c-app-engine` links that archive and turns it into
an `Application`, and `examples/c-app-sequencer` composes that with the sequencer library into a
host. Nothing else about the application reaches this repository.

**The seam is an ABI, not a language.** An engine only has to export these symbols with C linkage
and C layout. The reference engine, `examples/c-wallet-engine`, is written in Rust and exports
them as `libc_wallet_engine.a`, which is why this repository maintains no C beyond the header
itself.

## Why a C seam exists

An application whose canonical on-chain execution must stay free of Rust — a Cartesi Machine
binary whose template hash has to stay reproducible, for instance — still has to run inside the
sequencer host, which is Rust. Reimplementing the application twice is how a rollup diverges from
itself, and a divergence between the sequencer and the canonical machine is, under rollup
semantics, indistinguishable from theft.

The C seam removes the second implementation. One compiled engine is linked by the host through
this binding and by the application's canonical binary natively, so off-chain and on-chain
execution agree by construction rather than by review.

## The build contract

Three environment variables, and they are the whole binding.

| Variable | Meaning |
| --- | --- |
| `APPLICATION_ENGINE_LIB` | The static archive implementing the header. It must be self-contained: the shim links it and nothing else the engine needs. |
| `APPLICATION_ENGINE_HEADER` | The header the archive was built against. The FFI declarations are generated from it with bindgen, never restated by hand, so a signature the host cannot express fails the build instead of the runtime. |
| `APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT` | The application's largest method payload, in bytes. It is defined on the compile line of every consumer of the header, the engine's own translation units included, and becomes the sequencer's `MAX_METHOD_PAYLOAD_BYTES`. |

Supplying `APPLICATION_ENGINE_LIB` requires the other two: the archive and the header are separate
artifacts and only that pairing is meaningful, and a bound the build guessed would be exactly the
silently wrong number the header's `#error` exists to prevent. An engine that raised its own bound
without raising the host's would never see the payloads it grew to accept.

The generic binary in `c-app-sequencer` reads `APPLICATION_ENGINE_LIB` too, and reports that it
has no application to run when none was supplied. That is a build-script check rather than a cargo
feature, because features are additive and `--all-features` would otherwise turn the binary on in
builds that supplied no archive, which is exactly the combination that cannot link.

With none of the three set, `c-app-engine` links no archive at all. An rlib may carry undefined
symbols, so it still builds, and the binary using it supplies the engine instead: that is how
`c-wallet-sequencer` links `c-wallet-engine` as an ordinary crate dependency, letting the whole
path build and test under a plain `cargo build --workspace` with no archive path handed to it.
The payload limit then falls back to the wallet's own, which `c-wallet-engine` asserts against
`WalletApp::MAX_METHOD_PAYLOAD_BYTES` so a drift fails that crate's compile.

## What crosses, and what does not

Only scalars, fixed-width byte records and borrowed byte spans. Amounts cross as 32 big-endian
bytes rather than as a number the host might not be able to spell. Records are plain C layout and
an engine must static_assert their sizes and offsets, matching the compile-time assertions the
generated bindings carry, so a compiler laying one out differently fails a build rather than the
boundary.

**No configuration crosses inward, and there is no getter for it.** There is no create path: the
only constructor is `application_engine_from_dump`. A state is written once by a genesis tool the
application ships, and every engine afterwards opens what is there. The host therefore cannot be
told what application to be, and cannot ask. That is why `c-app-sequencer` takes exactly one
application-related argument, `--state-file`, and learns nothing from it.

Every vocabulary in the header — statuses, invalid reasons, output kinds — is append-only and no
value is ever reused. They cross as plain integers rather than as Rust enums, so a value an engine
adds later is a number the host refuses rather than undefined behavior.

## What a C engine owes

These are the obligations the trait's types would otherwise carry, and nothing enforces them at
runtime.

- **Determinism.** The same state and the same input stream must produce the same bytes, on every
  architecture the application runs on. Fixed-width fields and explicit byte order, no host-endian
  writes, no padding-dependent images.
- **No exception may cross.** Every entry point is `noexcept` under C++ and reports failure as a
  status. A throw reaching the seam terminates, and so does a Rust `panic!` reaching an
  `extern "C"` entry point, which is the same fatal-no-resume policy.
- **Validation is pure.** `application_engine_validate_user_op` mutates nothing. Catch-up replay
  depends on it.
- **Execution is self-sufficient.** Replay calls `application_engine_execute_valid_user_op`
  directly with no validation in front of it, so consuming the nonce and charging the fee happen
  there and never in validation.
- **Both clocks advance in execution.** `max(clock, safe_block)` and `max(clock, block_number)`,
  carried by execution rather than set, so an engine cannot execute and forget. They live in the
  state, so they survive the dump round trip recovery depends on.
- **Every input is counted**, including the ones that decode to nothing.
- **Every entry point is total over its payload bytes.** They are attacker-influenced on all
  three: a direct input is whatever was posted to L1, and a user op is whatever was signed and
  sent to `POST /tx`. A method the engine cannot parse is a refusal or a counted no-op, never
  `INTERNAL_ERROR` — that status aborts the host, so reporting it on unparseable input hands any
  caller a one-request process kill.
- **Single-threaded per handle, reentrant without one.** The engine's handle is never used by two
  threads at once, but the entry points that take no handle are called from request handlers while
  an execution is in flight — `state_file_in_dump` is reached from the snapshot routes. An engine
  answering out of one process-wide buffer races there; thread-local storage is the simple answer.

## The drain protocol

An execution reports how many outputs it produced and the caller takes exactly that many. Taking
one more is a caller bug reported as an internal error, never an empty output a host might act on,
and an engine refuses to execute while an earlier execution's outputs are still queued rather than
discarding outputs bound for the chain. That refusal is what makes a reported count belong to the
execution that reported it.

Payload pointers are borrowed and released by the next drain, so a host copies before draining
again.

## Errors

Errno style. Zero is success and every failure is negative, so `status < 0` is the failure test
and a status added later cannot disturb it. `IO_ERROR` and `INTERNAL_ERROR` are a real split, not
a nominal one: a full disk is a condition a caller may act on, an engine that failed its own
invariant is not.

The status is the contract and the accompanying message is a diagnostic. Never branch on its text.

An `INTERNAL_ERROR` **aborts the host process**, on every path including `state_file_in_dump`,
which the snapshot routes reach — the trait's signature there is infallible, so the host has
nowhere to put the error. Returning it would hand the error
to the canonical fold, which catches application errors and continues, while the same failure in
the application's own canonical binary terminates it — on state that may already be partially
mutated. The two must not disagree about that.

## Dumps

A dump is a directory the engine creates, and `state_file_in_dump` names a file inside it, which
is the shape the reference engine takes. An engine whose persistence representation already *is*
its canonical state may instead make the prefix that file, which the contract's "can be the same
file" concession allows and which the runtime supports today: it creates only the enclosing dump
directory, joins `state` onto it to form the prefix, and removes the dump directory recursively.
Nothing in this repository exercises that second shape, so an engine taking it should re-check the
runtime's behaviour rather than assume it.

`create_dump` must fsync the payload and the directory entry naming it before returning, so on
success the dump survives an immediate kernel crash. The sequencer inserts the row referencing the
dump after the call returns.

## Not carried

`canonical_snapshot_bytes` and `export_state` have no declaration in the header and stay
defaulted. The watchdog's `/finalized_state` compare reads the first, so an application needing
that comparison reaches its canonical bytes through the file `state_file_in_dump` names. Both
would need a header revision to cross.

## The reference engine

`examples/c-wallet-engine` exports `app-core`'s placeholder wallet through this API. It is the
same application `examples/wallet-sequencer` reaches directly as a Rust `Application`, so the two
binaries are the same wallet with and without the seam in between, which is what makes the seam's
cost and behaviour comparable rather than asserted.

It is written in Rust deliberately. Nothing about the contract requires C on the engine side, and
keeping the reference in Rust means this repository maintains no C beyond the header while still
exercising every rule stated above. An engine in C, C++, Zig, or anything else with a C ABI takes
exactly the same path; only `APPLICATION_ENGINE_LIB` differs.

**Nothing exercises the seam at runtime.** The workspace build links the reference engine into
`c-wallet-sequencer`, so a signature the two sides disagree about fails to compile, but no test
makes a call across the ABI. The rules above that carry no runtime enforcement — the drain count,
the refusal to execute over queued outputs, purity of validation, the error-message lifecycle —
are held by review alone. An engine author should assume the same of their own and test
accordingly.
