# Review follow-up validation

Evidence for the ongoing stack review and landing, covering changes above
`f504c2e88e2b432718493e29e3c56f74fce3ad00` on `codex/stack-review-fixes`.
Local validation below includes the filesystem-identity fix at
`8b6da04374c7a4aef61fba8e7963e71d3cd55122`, CI diagnostics at
`93e6a49ad19c35fe78f4c46d639eb8fd556c58ba`, and the accompanying documentation
clarifications. Those clarifications change no executable behavior.
Retire this record after landing when no ongoing review decision uses it.

## Changes and discriminating checks

- Startup compares filesystem device/inode identities after inspecting every
  retained reference. Regressions preserve relative/absolute, leading-dot,
  symlink, and macOS firmlink aliases, including mixed literal/aliased references,
  while removing genuine orphans. The firmlink test fails against the old
  canonical-path comparison because it deletes the registered artifact, and
  passes with identity matching. A missing reference stops the sweep before any
  deletion. Dangling orphan links are removed without following their targets.
  All eleven startup hygiene tests pass. The firmlink regression runs on macOS;
  no Linux bind-mount scenario was run locally.
- A scheduler/fold regression establishes that `A < B` can coexist with a
  pending direct at `A` after a same-block frame and a later empty batch. It
  verifies recovery from an eligible earlier checkpoint preserves every direct,
  application progress, and the scheduler nonce. The
  [checkpoint contract](../recovery/cockroach.md#checkpoint-eligibility) requires
  canonical exporters to inspect the pending queue and refuse such candidates.
- The accepted-prefix fixture now places the old future-nonce input both below
  and exactly at `C`, before the nonce-0 batch in L1 order. Temporarily changing
  the production query from `> C` to `>= C` fails specifically at the equality
  case. Restoring `>` passes all four acceptance-projection tests.
- API documentation states the existing address-casing/normalization contract,
  WS admission statuses, health-probe semantics, and internal deployment
  boundary. Writer ownership and stale source references were corrected. The
  submitter's own-sender decode error remains visible under self-trust.
- CI retains harness logs from failed rollups E2E jobs for seven days. This adds
  evidence for the unclassified C-host recovery WebSocket reset in the
  [register](register.md), without changing test or recovery behavior.

Separate reviewers examined snapshot cleanup and checkpoint eligibility. Review
caught the dangling-orphan-link case before final validation; its regression is
included. No findings remain within the implemented scope.

## Validation

macOS arm64, parent Nix/direnv environment, Rust and Cargo 1.95.0:

| Check | Result |
|---|---|
| `cargo test --locked --workspace --exclude canonical-test -- --test-threads=1` | 760 passed; zero failed; one existing ignored harness doc test |
| `cargo check --locked --workspace --all-targets` | Passed |
| `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | Passed |
| `cargo fmt --all -- --check` and `git diff --check` | Passed |
| Relative Markdown links and code fences across repository Markdown files | 409 targets/anchors checked |
| CI workflow YAML parsing | Passed; artifact upload itself requires a failed GitHub job |

The full host suite includes the new regressions. Guest execution, standalone
rollups E2Es, watchdog Lua tests, and TLA+ model runs were not repeated for this
follow-up. Both current recovery models were read; their modeled transitions
are unchanged. Earlier validation remains in the
[initial closeout record](2026-09-18-stack-review-validation.md).

## Remaining integration and landing work

The bundle carries no canonical queue evidence, and no canonical-to-native
exporter is implemented here. The eligibility change is a supported-checkpoint
precondition and executable scheduler regression, not loader enforcement or a
completed CM export drill. The application's exporter must enforce refusal and
demonstrate fallback and eligible pending-queue recovery; the
[integration plan](../plans/2026-07-coordination-tracks.md#track-6--dump--application-api-redesign)
tracks that work. Canonical acceptance semantics and artifact formats are unchanged.

Archive concurrency limits remain deferred pending deployment workload needs.
The existing process-lock concurrency investigation remains open; this host run
used the serial suite. The C-host WebSocket reset is a separate unresolved
investigation; one successful run cannot classify its cause.

The lower-stack ancestry is reconciled: PR #38 is at
`7f3229f2e42585e055f2fabb8e280c4d42dd5a81`, and GitHub reports PR #42 mergeable
at `35697691d7a5ba5a2c868f51b3a45c3dd5b6ee44` (checked 2026-09-19).
Consult CI for the pushed revision before landing; local host checks do not
replace guest, watchdog, or standalone rollups E2E execution.
