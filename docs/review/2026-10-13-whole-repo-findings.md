# Whole-repo review, 2026-10-13: findings appendix

Generated from the read-only review fleet at `1a9615d`. The plan and triage live in [the review record](2026-10-13-whole-repo-review.md). Delete this file with the record.

Status: **confirmed** or **partially** (the claim shown is the skeptic's correction) survived an adversarial skeptic; **disputed** means an independent trace refuted a skeptic-confirmed claim; **unverified** and **skeptic-added** are single-source. Line deltas are estimates (negative = removal). BC = behavior change.


## A. Application trait, core domain and wire types, C ABI (consumer surface: app integrators)

Verdict: This area is the integrator surface. It has three parts: the Application trait with its shared execution boundary (about 350 production lines), the wire and domain types (UserOp, SignedUserOp, WireUserOp, Batch, Frame and the history identity types, about 410 lines), and the C ABI bridge (header, adapter and build.rs, about 835 lines, of which about 280 are comment lines in the header). The Rust core is proportionate. The trait is small, the outcome and error types are minimal and covered by tests, and the progress check costs two progress() reads per input while guarding I19 (silent application-history divergence). The problems are at the seam, not in the logic. The header is about 70% prose and repeats an application contract that is already stated in three other places. Two host concerns leak into every engine: rejection reasons only the boundary can produce, and an obligation to fsync a directory the host owns. The stateful drain protocol and the path callback (Track 6) are what a C++ integrator will trip on; they should be decided in one 'seam v1' change with Bart, together with N8, before libdex freezes its header. history.rs's era/generation identity (about 180 lines, plus history_api.rs and broadcast.rs) serves the subscribe-across-recovery egress model that the new direction replaces, so it should be frozen. I found no scheduler/sequencer divergence bug: both sides use validate_and_execute_user_op, build_input_domain and Signature::from_raw over the same canonical 65 signature bytes. Keep the engine-owned progress, the integer-constant enums that refuse unknown values, and fees crossing the ABI as log-space exponents.


### A1. Rejection reasons only the boundary can produce leak into the app's and the C ABI's vocabularies

`medium · confirmed · -40 lines · S · c-abi`  
Where: `sequencer-core/src/application/mod.rs:111-131`, `sequencer-core/src/application/mod.rs:169-174`, `sequencer-core/src/application/mod.rs:278-294`, `bindings/c-app-engine/include/application-engine.h:141-148`, `bindings/c-app-engine/include/application-engine.h:172-191`

- Claim: As stated. Two additions strengthen it. First, AGENTS.md:216 tells integrators that every app must implement NonceExhausted and InvalidMaxFee, which contradicts mod.rs:165-167 and header:300-302. Second, the scheduler's test double RecordingApp (sequencer-core/src/scheduler/mod.rs:503-510) re-implements the max-fee guard and returns InvalidMaxFee, which the boundary at mod.rs:279 makes dead. With AppRejection that becomes a type error. The net line delta is nearer -25 than -40, because the AppRejection enum and its conversion add about 15 lines.
- Fix: Have validate_user_op return Reject(AppRejection), where AppRejection has only InvalidNonce and InsufficientFeeBalance. The boundary wraps it into InvalidReason (an App(..) variant plus InvalidMaxFee and NonceExhausted, or a flat enum with From<AppRejection>), and the Display text stays the same. In the header, delete the two codes and ApplicationEngineInvalidMaxFee, marking values 1 and 3 as reserved, and pass the engine only (nonce, data) for validation. With that, the type makes the misuse impossible.
- Decision: C ABI change: agree it with the libdex owner and land it in the same seam-v1 change as the Track 6 items and N8.

### A2. Every engine must fsync a directory the host owns

`medium · confirmed · -6 lines · S · BC · application-trait`  
Where: `sequencer-core/src/application/mod.rs:234-241`, `bindings/c-app-engine/include/application-engine.h:362-364`, `docs/protocol/application-contract.md:207-211`, `sequencer/src/ingress/inclusion_lane/dump_info.rs:136-148`

- Claim: As stated. Every production checkpoint write goes through create_dump_dir_with_info: setup/mod.rs:799,844,901, setup/fill.rs:62 and inclusion_lane/snapshot.rs:51. The host fsyncs dump_dir only in write_info (dump_info.rs:170), which runs before app.create_dump creates the `state` entry (dump_info.rs:143). It then fsyncs the dumps directory (dump_info.rs:144-147). The `state` dentry therefore depends on the engine today. The wallet does it at wallet.rs:383-385, under a wrong comment: 'dumps_dir/ contains prefix's dentry' at wallet.rs:382, where the parent is actually the per-checkpoint dump_dir. Once the host fsyncs dump_dir, the wallet can drop those three lines.
- Fix: Add File::open(dump_dir)?.sync_all()? right after app.create_dump in create_dump_dir_with_info. Shrink the engine contract to 'fsync everything you wrote at and beneath prefix', and move the WAL-ordering rationale into dump_info.rs. Update the trait doc, the header and application-contract.md to match.

### A3. The Application contract is stated in four places, and the header is about 70% prose

`medium · partially · -180 lines · M · docs-only`  
Where: `bindings/c-app-engine/include/application-engine.h:9-68`, `bindings/c-app-engine/include/application-engine.h:107-123`, `bindings/c-app-engine/include/application-engine.h:186-187`, `bindings/c-app-engine/include/application-engine.h:356-395`, `sequencer-core/src/application/mod.rs:124-126`

- Claim: The cross-cutting duplication is real. The nonce rule appears at AGENTS.md:216-217, header:53-57, contract:96-120 and mod.rs:176-180. The fee conversion appears at header:59-63 and c-application-binding.md:39-46. Dump obligations are restated in mod.rs:225-251, header:356-378, contract:207-234 and README:25-34. The design-rationale lines can go: header:107-109, 115-116, 121-123 and 186-187. But the header cannot shrink to a 10-line preamble. It is the self-contained ABI for bindgen and C++ consumers ('the header carries the compatibility obligation on its own', header:31-34), and c-application-binding.md:37 defers buffer lifetimes to it. The per-entry ownership, lifetime, reentrancy and status rules must stay in the header (header:255-266, 318-320, 339-344, 391-393). README:119-124 is a runnable recipe, not a test inventory, so keep it. batch.rs:7-39 duplicates the batch_policy comment in 0001_schema.sql:557-606, and it describes the CHECK as a linear 'batch_size_target < const_max_batch_bytes' when the actual constraint is log-space (0001_schema.sql:587). A realistic delta is about -100 to -130.
- Fix: Make application-contract.md the only owner. The header keeps a preamble of about 10 lines that links the contract sections, plus a terse @pre/@post/@returns per entry point. The trait docs shrink to one or two lines per method with a section link. The README drops the restatement at 13-34 and the test inventory at 105-129 (link the tests instead). batch.rs keeps two lines and a link to fee.rs.

### A4. Freeze the era/generation history-identity types; they serve the egress model being replaced

`low · partially · +0 lines · L · http-api`  
Where: `sequencer-core/src/history.rs:45-223`, `sequencer-core/src/history_api.rs`, `sequencer-core/src/broadcast.rs:28-88`, `docs/protocol/application-contract.md:152-155`

- Claim: Freezing is fine: no uuid-crate swap and no reworked wire codes for EraId (history.rs:45-139) or HistoryPolicyError (history.rs:212-223). But RecoveryGeneration and the generation cuts are I18 machinery for standard recoveries, not cockroach ones. That covers the exact +1 generation trigger, history_generation_cuts and cascade_and_reopen (docs/invariants.md I18). The new direction removes subscribing across a rebuild, which is an era change. Any future sync must still detect a standard-recovery suffix replacement while a client is subscribed (I18 'Breaks: a client silently resumes a replaced suffix'), and the Do-not-simplify list forbids keeping a numeric resume offset without its history identity. Catch-up also pages through the shared claim-based reader (catch_up.rs:53-72). So the ~-350-line deletion does not follow from the direction. At most EraId's wire role shrinks, and generation will probably survive in some form.
- Fix: Freeze these types and do not polish them: no uuid-crate swap, no reworked error codes. Keep ExecutedInputCount (history.rs:10-43), which is application progress, and move it next to ApplicationProgress when history.rs is retired. Retiring the module requires re-homing the catch-up's identity check.
- Decision: Confirm that the new sequencer<->client sync retires era/generation claims; the deletion (about -350 lines) waits for that design.

### A5. Overridable progress shortcuts on the trait can disagree with progress()

`nit · partially · -15 lines · S · application-trait`  
Where: `sequencer-core/src/application/mod.rs:198-213`, `examples/app-core/src/application/wallet.rs:159-161`

- Claim: Overridable default methods do exist (mod.rs:203-213) and the wallet shadows one inherently (wallet.rs:159-161). But no implementation overrides them. Rust application code is self-trusted (application-contract.md:6-9, I19 'Scope'), so an override that disagrees with progress() is an app bug inside the trust boundary, not an unverified path. An extension trait would not leave call sites unchanged either: every calling module would need to `use` the new trait. The cheap, worthwhile part is deleting the wallet's inherent shadow and the duplicated doc text.
- Fix: Move the two methods to an extension trait with a blanket impl (impl<A: Application> AppProgressExt for A), so they cannot be overridden and call sites stay unchanged. Drop the duplicated doc text and the wallet's inherent shadow.

### A6. The adapter turns lifecycle statuses from execute, validate or drain into retryable I/O errors

`low · confirmed · +4 lines · S · BC`  
Where: `bindings/c-app-engine/src/lib.rs:70-85`, `bindings/c-app-engine/include/application-engine.h:47-49`, `docs/protocol/application-contract.md:86-90`

- Claim: As stated. Note that the reference engine blurs the same line on its own side: its shared report() (c-wallet-engine lib.rs:52-70) maps Io kinds to NOT_FOUND or INVALID_DUMP for validate, execute and drain alike. The lane classifies any AppError::Io from ExecuteUserOp or ExecuteDirectInput as non-terminal (sequencer/src/ingress/inclusion_lane/error.rs:57-59,71-73), so a contract-violating status causes a restart loop instead of a terminal stop.
- Fix: Pass a lifecycle flag to check(), set only by from_dump and create_dump. Everywhere else, map NOT_FOUND and INVALID_DUMP to AppError::Internal with the status in the reason.

### A7. The frame fee has four names across the integrator surface

`nit · unverified-nit · +0 lines · S · c-abi`  
Where: `sequencer-core/src/application/mod.rs:118-121`, `sequencer-core/src/application/mod.rs:173`, `sequencer-core/src/batch.rs:58`, `sequencer-core/src/l2_tx.rs:18`, `bindings/c-app-engine/include/application-engine.h:156`

- Claim: The same quantity is called current_fee (trait and validate ABI), fee_price (Frame), fee (ValidUserOp, and the header's ValidUserOp), and base_fee (InvalidReason and ApplicationEngineInvalidMaxFee). 'nonce' likewise has three meanings: user op, batch and L1 wallet (see N14).
- Fix: Use one term (for example frame_fee) in Rust and C. The SSZ field name does not affect the wire format. The header field rename is source-breaking for engines, so do it in the seam-v1 change and in the same naming pass as N14.
- Decision: Choose the term; whether the /tx rejection Display text may change.

### A8. The scheduler builds the same UserOp twice per op

`nit · unverified-nit · -2 lines · S`  
Where: `sequencer-core/src/scheduler/mod.rs:333-334`, `sequencer-core/src/scheduler/mod.rs:350-356`, `sequencer-core/src/batch.rs:73-79`, `sequencer-core/src/application/mod.rs:296-300`

- Claim: recover_sender calls to_user_op() (which clones data), the caller calls to_user_op() again, and the boundary then copies data into ValidUserOp: three payload copies per op in the canonical machine. The cost is negligible next to ecrecover; the point is that the wire-to-signed conversion is spelled twice.
- Fix: Build the UserOp once and pass it to recover_sender. Fold this into the N7 change.

### AS1. AGENTS.md tells every app to implement the two rejections only the boundary may produce

`low · skeptic-added · +0 lines · S · docs-only`  
Where: `AGENTS.md:216`, `sequencer-core/src/application/mod.rs:165-167`, `bindings/c-app-engine/include/application-engine.h:299-302`, `bindings/c-app-engine/src/lib.rs:193-197`, `sequencer/src/ingress/inclusion_lane/error.rs:71-73`

- Claim: AGENTS.md:216 lists InvalidNonce, NonceExhausted, InvalidMaxFee and InsufficientFeeBalance as 'protocol-level rejection semantics every app must implement'. The trait doc and the header say the opposite: the boundary owns the max-fee and exhausted-nonce guards, and an engine never returns them. A C engine that follows AGENTS.md and reports NONCE_EXHAUSTED for an account at u32::MAX gets AppError::Internal from the adapter. The lane classifies that as terminal (error.rs:71-73; validate errors surface as ExecuteUserOp at inclusion_lane/mod.rs:401).
- Fix: Reword AGENTS.md:216: apps reject only InvalidNonce and InsufficientFeeBalance, and the shared boundary adds InvalidMaxFee and NonceExhausted. This is a one-line doc fix, independent of A1's ABI change, and can land now.

Keep (load-bearing):

- Shared execution boundary with a progress assert (sequencer-core/src/application/mod.rs:324-349): two progress() reads per input catch the silent application-history divergence I19 describes, and the overflow preflight runs before any mutation. This is proportionate.
- Engine-owned progress instead of a host mirror or AppWithProgress wrapper (an alternative the jury refuted): cockroach recovery must recover the count and clock from the canonical checkpoint (docs/protocol/application-contract.md:244-249), so they belong in the app's state.
- ApplicationProgress::try_new (mod.rs:62-73): the single coherence check for imported progress (dumps and C engines); it fails loud in the adapter (bindings/c-app-engine/src/lib.rs:250-254).
- ExecutedInputCount newtype with checked arithmetic (sequencer-core/src/history.rs:10-43): stops the app-history offset being confused with SQLite cursors or advanced with wrapping math.
- Separate ValidationOutcome and ExecutionOutcome enums: named outcomes read better than a nested Result<Result<..>> and keep rejection distinct from fatal AppError.
- AppError with exactly Internal and Io: the terminal-versus-retryable split that startup classification relies on (application-contract.md:86-90, 230-234).
- Fees crossing the ABI as log-space exponents, with the linear conversion inside the engine: the same engine code runs natively and canonically, so there is one conversion implementation per engine. Passing a linear amount from the host would add a second implementation that must stay bit-exact.
- Max-fee guard before app validation and the u32::MAX guard after it (mod.rs:278-294): this order gives correct diagnostics, tests pin it, and the sequencer and scheduler share it.
- C enums generated as integer constants that refuse unknown values (bindings/c-app-engine/build.rs:29-31; src/lib.rs:128-132,194-197): drift between engine and host becomes a loud error instead of undefined behavior.
- EngineApp is Send but not Sync or Clone (bindings/c-app-engine/src/lib.rs:87-94): matches the lane's single owner of the engine; an independent fork goes through checkpoint and restore.
- create_dump/from_dump isolation and durability contract (minus the parent fsync, see A2): checkpoint GC (I13) and snapshot leases depend on it.
- SSZ decode-robustness tests on adversarial bytes (sequencer-core/src/batch.rs:163-217): the scheduler decodes L1 input, so this pins totality.
- The scheduler silently dropping ops whose signature cannot be recovered (sequencer-core/src/scheduler/mod.rs:330-333): the fold must stay deterministic and free of side effects.

## B. Scheduler and protocol spec (consumer surface: scheduler reimplementers)

Verdict: Area B is the canonical scheduler: `Scheduler<A>` (~420 production lines), the recovery fold `fold_replay` (~150), the off-chain mirror in protocol.rs (~455), the guest harness (~140) and a 302-line spec. The fold itself is small, correct and proportionate. I found no Byzantine path that breaks it. Gate d runs before staleness, so N9 is fixed; the fridge drains in order, the nonce check runs before any state changes, and stale or rejected batches never consume a nonce. The size problems are next to the core, not in it. Only about 60 of protocol.rs's ~455 production lines mirror the scheduler; the rest is sequencer-local danger and wall-clock policy, plus re-validation asserts needed only because the struct fields are public. About half of scheduler-semantics.md is sequencer architecture repeated from I1/I2/I3/I19. Meanwhile the spec leaves out what a C++ port needs most: the SSZ schema, how the EIP-712 domain is built, and the signature acceptance rules. Today those rules are whatever alloy 1.6.1 does (v byte in {0,1,27,28,35..=255}, high-s normalized and accepted). That is the biggest risk before the port. The most valuable code change is to have `scheduler_accepts` mirror gate d. That removes the one documented exception to I1, along with the false positive in I9, and turns a self-bug that today causes silent divergence into a visible stall. `fold_replay` is already shaped like the future standalone recovery tool (pure, takes iterators, trusts the checkpoint nonce). Keep it and its fail-loud asserts, and add the missing `inclusion_block <= C` assert.


### B1. Spec omits wire, domain and signature rules a port needs; the signature rules come from alloy, not from the spec

`medium · partially · +70 lines · S · docs-only`  
Where: `docs/protocol/scheduler-semantics.md:63-111`, `sequencer-core/src/scheduler/mod.rs:350-358`, `sequencer-core/src/batch.rs:46-68`, `sequencer-core/src/lib.rs:21-43`, `examples/canonical-app/src/scheduler/mod.rs:33-35`

- Claim: The spec does leave out the SSZ layout, the EIP-712 domain (lib.rs:23-43), the UserOp type, the MAX_WAIT value (1200, lib.rs:21) and the signature rules. The acceptance set really is alloy 1.6.1's: from_raw requires exactly 65 bytes, normalize_v accepts 0|1|27|28|35.., and both recovery backends call normalized_s() (sig.rs:380, sig.rs:407). The honest-path risk is narrower than claimed, though. Storage re-encodes every signature with Signature::as_bytes (sequencer/src/storage/ingress.rs:723), and as_bytes writes v_byte(), which is 27 or 28. So sequencer-built batches always carry v in {27,28}, and the wider v set only matters for batch bytes this sequencer did not produce. as_bytes copies s as-is (sig.rs:174-179), so user-supplied high-s does reach batches. High-s is the live divergence hazard for a port that enforces low-s. This is a documentation gap that guards a future implementation, not a present bug.
- Fix: Add a normative 'Wire and signature rules' section (~40 lines): the SSZ containers and field order, the domain, the UserOp EIP-712 type, the accepted v set, the high-s rule, how a wrong-length signature is handled, and the constants. Add a pinned test-vector table to the scheduler tests (v = 0, 1, 27, 28, 35, 255; high-s; a 64-byte signature) so both an alloy bump and the C++ port are checked against it. Separately, decide whether to narrow the accepted set (for example v in {27,28}, low-s only) while the port forces a new machine anyway.
- Decision: Should the canonical signature acceptance set be narrowed (a consensus change needing a new machine image) or documented exactly as it is?
- Independent trace: confirmed. The code facts check out: from_raw accepts any s, recovery calls normalized_s() first, and storage re-serializes s exactly as received.

### B2. scheduler_accepts should mirror gate d; dropping the 'documented omission' simplifies I1 and I9

`medium · disputed · -40 lines · S · BC`  
Where: `sequencer-core/src/protocol.rs:353-388`, `sequencer-core/src/scheduler/mod.rs:295-317`, `sequencer-core/src/scheduler/mod.rs:1513-1539`, `docs/protocol/scheduler-semantics.md:205-213`, `docs/invariants.md:100-105`

- Claim: The off-chain predicate skips the structural checks on a self-trust argument, but in the self-bug case it is the predicate that fails silently. Suppose an own batch has a frame with safe_block > inclusion_block or decreasing safe_blocks. The canonical scheduler rejects it without consuming the nonce. `scheduler_accepts` accepts it, and the content-identity check returns Match because these are our own bytes. The frontier advances while every later own batch gets WrongNonce on chain: silent, growing divergence. Conversely, a foreign batch that is structurally invalid triggers a false CanonicalDivergence and forces a manual cockroach recovery. `batch_reject_reason_for_block` never uses `&self`.
- Fix: Make it a free `pub fn batch_reject_reason(inclusion_block, frames) -> Option<BatchRejectReason>` in scheduler/mod.rs and call it from `scheduler_accepts` after decoding. Then delete the exception prose in four places (protocol.rs:353-362, spec 205-213, I1 parenthetical, I9 false-positive sentence), turn the special-case block in the duality test into ordinary agree cases, and simplify the l1_submission test. I1 then holds with no exceptions.
- Decision: This reverses a deliberate self-trust omission (needs maintainer sign-off). The added cost is only an O(frames) check.
- Independent trace: refuted. The asymmetry is real and pinned by tests.

### B3. fold_replay claims it enforces 'everything <= C' fail-loud but never checks replay batches against stop_block

`low · partially · +6 lines · S · BC`  
Where: `sequencer-core/src/scheduler/fold.rs:79-85`, `sequencer-core/src/scheduler/fold.rs:107-133`, `sequencer-core/src/scheduler/fold.rs:140-150`

- Claim: The comment at fold.rs:79-81 claims 'everything <= C' is enforced fail-loud, but nothing compares replay or seed blocks against stop_block. A replay batch above C is folded in silently. A direct above C also escapes the post-drain fridge assert (fold.rs:144-150) whenever a later batch above C has a frame that drains it. The only production caller bounds its inputs (setup/mod.rs:488 and :494 use safe_inputs_in_block_range(.., stop_block)), so nothing reaches this today. It is a comment overclaim plus a missing cheap assert.
- Fix: Add `assert!(input.inclusion_block <= stop_block, ...)` in the replay loop, and the same for seeds. The post-drain fridge assert then becomes a consequence of these checks; keep it as a cheap backstop.
- Independent trace: confirmed. The code matches the finding. fold.rs:79-81 says the input contract "everything `<= C`" is "enforced fail-loud".

### B4. protocol.rs is mostly sequencer-local danger policy shipped inside the guest-shared crate

`low · partially · -10 lines · M · public-rust-api`  
Where: `sequencer-core/src/protocol.rs:4-24`, `sequencer-core/src/protocol.rs:30-97`, `sequencer-core/src/protocol.rs:185-324`, `sequencer-core/src/protocol.rs:326-454`

- Claim: protocol.rs does mix about 60-100 lines that mirror the scheduler (age_exceeds, is_scheduler_stale, scheduler_accepts, SafeInputView, AcceptedBatch) with about 300 lines of sequencer-local timing policy. The struct is a deliberate 'single source of truth' (protocol.rs:7-18) that puts the mirrored max_wait_blocks next to the local fields. Splitting it means changing scheduler_accepts' signature and touching roughly 20 consumer files across sequencer/src and tests/e2e (tests/e2e/src/test_cases.rs:52 reads FRAME_CLOCK_INTERVAL_SAFE_BLOCKS from sequencer_core::protocol). The net delta is about -10 lines and no defect is fixed.
- Fix: Pure move, no behavior change. Move the danger, wall-clock and L1-view timing plus their errors into the sequencer crate (for example sequencer/src/recovery/timing.rs). Move `scheduler_accepts(max_wait, submitter, input, expected)` next to `Scheduler` (scheduler/predicate.rs) so all scheduler-mirroring code sits in one module. Do it in a separate patch from B2 and B6.

### B5. advance_expected_batch_nonce is described as a gold-frontier scheduler mirror, but it is a submitter heuristic

`low · confirmed · -25 lines · S · docs-only`  
Where: `docs/protocol/scheduler-semantics.md:197`, `docs/protocol/scheduler-semantics.md:222-230`, `docs/invariants.md:110-114`, `sequencer-core/src/protocol.rs:399-435`, `sequencer/src/l1/submitter/worker.rs:99-108`

- Claim: The spec table row #2 says that `scheduler_accepts` plus `advance_expected_batch_nonce` 'builds the gold frontier'. In fact the frontier uses an inline copy, and the function's only consumer is `decide_submit_start`. That caller folds latest (not safe) events and applies no staleness, so the function does not mirror the scheduler's nonce rule (a stale batch does not consume its nonce on chain).
- Fix: Move the function into submitter/worker.rs as a private helper and delete the spec rows 197 and 222-230 and the I1 bullet at 110-114. The scheduler spec then describes only the scheduler.

### B6. Public ProtocolTiming fields force re-validation asserts in every accessor, plus five tests for them

`medium · confirmed · -90 lines · S · public-rust-api`  
Where: `sequencer-core/src/protocol.rs:103-106`, `sequencer-core/src/protocol.rs:137-142`, `sequencer-core/src/protocol.rs:191-198`, `sequencer-core/src/protocol.rs:205-212`, `sequencer-core/src/protocol.rs:222-225`

- Claim: `try_new` validates every field relationship once, but the fields are public 'to keep test fixtures concise'. So five methods re-assert the same invariants at runtime, and five tests (`*_fails_loud_on_*_direct_construction`) cover those asserts. A struct that cannot be built invalid makes all of this unnecessary.
- Fix: Make the fields private, add a `max_wait_blocks()` getter, and switch the ~8 fixtures to `try_new(..).expect(..)`. Delete the five assert blocks, the five fails-loud tests and the two 'fields stay public' comments.

### B7. Three spellings of one staleness predicate, plus alias re-exports

`low · confirmed · -30 lines · S · public-rust-api`  
Where: `sequencer-core/src/scheduler/mod.rs:17`, `sequencer-core/src/scheduler/mod.rs:409-419`, `sequencer-core/src/scheduler/mod.rs:73-88`, `sequencer-core/src/protocol.rs:391-397`

- Claim: `has_elapsed_since(start, wait, current)` wraps `age_exceeds(current, start, wait)` with the arguments reversed, and needs a doc comment to warn about the reversal. `age_exceeds` names its second parameter `first_frame_safe_block` even though the backstop passes a direct's inclusion block (mod.rs:387-391). `scheduler::MAX_WAIT_BLOCKS` and `input_domain` are pure aliases of `crate::MAX_WAIT_BLOCKS` and `build_input_domain`. Cross-type `PartialEq` exists only as test sugar.
- Fix: Keep a single `age_exceeds(now, start, window)` with neutral parameter names and delete `has_elapsed_since` and both aliases. Tests should compare `.outcome`. This merges spellings of the same formula only; it does not unify the two staleness references on the Do-not-simplify list.

### B8. QueuedDirectInput duplicates DirectInput; two near-identical drain loops each clone the payload

`low · confirmed · -25 lines · S`  
Where: `sequencer-core/src/scheduler/mod.rs:115-120`, `sequencer-core/src/scheduler/mod.rs:360-406`, `sequencer-core/src/scheduler/mod.rs:1308`

- Claim: The fridge stores {sender, payload, inclusion_block} and then rebuilds an identical `DirectInput` on every drain. Each drain loop clones the payload before popping, which momentarily doubles a payload the fridge is already holding (see the register's fridge-memory entry). AppError is fatal everywhere, so the queue state after an error has no meaning, and popping first is safe.
- Fix: Queue `DirectInput` directly and add one `drain_front_while(pred)` helper that pops, then executes, with no clone. Update the 1308 assertion. This is not the refuted retain-over-expect change.

### B9. Zero-frame batches consume a nonce at any age, and the sequencer never sends one

`low · confirmed · -10 lines · S · BC · wire`  
Where: `sequencer-core/src/scheduler/mod.rs:250-256`, `sequencer-core/src/protocol.rs:373-377`, `docs/protocol/scheduler-semantics.md:72`, `AGENTS.md:86`

- Claim: Gate c accepts `frames == []` regardless of age and consumes the nonce, so every implementation, the C++ port included, must carry this special case, and the predicate needs `unwrap_or(0)` plus an `!is_empty()` guard. The lane always opens a batch with a frame (I3: batch closure creates the successor's first frame), so the rule serves no honest traffic. Its only effect is a nonce consumer that never expires.
- Fix: If the C++ port forces a new canonical machine anyway, consider making empty `frames` a structural reject in gate d. Staleness then always has a first frame, and gate c disappears. Otherwise leave it and only document it.
- Decision: Changing the empty-batch rule is a canonical acceptance change. Is it worth doing now, alongside the port's new machine?

### B10. Spec does not say gate d is within-batch only, or that the reject reason is diagnostic

`low · confirmed · +4 lines · S · docs-only`  
Where: `docs/protocol/scheduler-semantics.md:78-85`, `sequencer-core/src/scheduler/mod.rs:305-314`, `sequencer-core/src/application/mod.rs:91-99`

- Claim: 'non-decreasing across the batch' could be read as a check against previous batches. In fact no cross-batch check exists: a later batch may use a lower safe_block, and user ops then see a non-monotone `safe_block`, while application progress clamps it with max. A port that adds a cross-batch check, or that depends on the per-frame interleaved order of reasons, would diverge or over-specify.
- Fix: Two spec sentences: 'monotonicity is checked within a batch only; no cross-batch state' and 'the reject reason is diagnostic; consensus only distinguishes accept/skip/reject'. Apps must not assume user-op safe_block is monotone across batches.

### B11. scheduler-semantics.md: about half is sequencer architecture repeated from the invariants

`medium · partially · -120 lines · M · docs-only`  
Where: `docs/protocol/scheduler-semantics.md:122-148`, `docs/protocol/scheduler-semantics.md:189-237`, `docs/protocol/scheduler-semantics.md:255-277`, `docs/protocol/scheduler-semantics.md:281-302`

- Claim: Two sections are real redundancy and can go: 'Reference-scheduler count discipline' (281-302), which restates the table at 163-171 and I19 and carries process notes, and the test-name inventory (255-277). All 11 test names in that inventory exist today, so 'will go stale' is a forecast, not an observed problem; test vectors would replace the list. The other two sections are not copies of invariants. The frame-clock section (122-148) is the designated prose owner, linked from invariants.md:146, AGENTS.md:265, application-contract.md:129 and protocol.rs:132. 'The three implementations' (189-237) is I1's authoritative prose (invariants.md:96) and is linked from AGENTS.md:79. Those two must be moved into sequencer-side docs with their links updated, not deleted. Realistic net is about -50 lines for the spec, plus relocation.
- Fix: Restructure into a normative porter spec of ~130 lines: inputs and metadata mapping, state (nonce, fridge), wire (B1), gates, drain, outputs, constants, test vectors. Add a five-line 'sequencer mirrors' pointer to I1/I2/I3. Delete the test-name inventory; the test vectors from B1 replace it.

### B12. N16: MAX_WAIT_BLOCKS is a crate constant hidden behind SchedulerConfig::new

`low · partially · +10 lines · M · db-schema`  
Where: `sequencer-core/src/lib.rs:19-21`, `sequencer-core/src/scheduler/mod.rs:17`, `sequencer-core/src/scheduler/mod.rs:29-39`, `examples/canonical-app/src/bin/canonical-app-devnet.rs:11`, `examples/canonical-app/src/bin/canonical-app-sepolia.rs:11`

- Claim: MAX_WAIT_BLOCKS is one constant (lib.rs:21) that the guest (through SchedulerConfig::new), run (config.rs:76) and the recovery fold (setup/mod.rs:591) all read, so they agree by construction within a source tree. The proposal would make it an operator-supplied setup input stored in DeploymentIdentity. That is a db-schema change, and it creates a new way for the sequencer's window to drift from the guest's compiled window, the same drift class as the submitter address (see BS1). No observed need for per-network windows was shown. The only cheap, uncontroversial part: test fixtures hardcode 1200 at detector.rs:198, l1_submission.rs:511 and storage/recovery_tests.rs:2771 (the last one is missing from the evidence list) and should reference the constant.
- Fix: Treat max_wait_blocks like sequencer_address. Guest: put per-network constants next to DEVNET/SEPOLIA_SEQUENCER_ADDRESS in app-core, and have the bins build `SchedulerConfig { sequencer_address, max_wait_blocks }` explicitly. Sequencer: setup takes it as a required input, records it in DeploymentIdentity, and run and the recovery fold read it from there (`ProtocolTiming::try_new(identity.max_wait_blocks, ..)`). Delete `crate::MAX_WAIT_BLOCKS`, the alias and `SchedulerConfig::new`. Replace the test literals with a test constant. Optionally, later, the guest's 'state' inspect could also report its config so setup can cross-check it; only add that if a mismatch is observed in practice.
- Decision: Should setup take max_wait_blocks as a required CLI input (no default), and how is the guest value pinned (per-network bin constant vs a rootfs config file)?

### B13. Guest reports and fold asserts leave out the context needed under error

`low · partially · +10 lines · S`  
Where: `examples/canonical-app/src/scheduler/mod.rs:44-57`, `sequencer-core/src/scheduler/fold.rs:90-93`, `sequencer-core/src/scheduler/fold.rs:117-125`

- Claim: Confirmed facts: a fixed reason-less report string (canonical-app/src/scheduler/mod.rs:51-57), no report for stale skips, no block or sender in the fatal panic at :46, and no values in the fold assert messages at fold.rs:92 and :119. Correction: reports are not part of the outputs tree, so this is consensus-neutral, but any change to the guest binary produces a new canonical machine image (template hash). It cannot ship to an existing deployment on its own and should ride with the next canonical-machine change (the C++ port or the fridge bound). The fold-assert message changes are host-only and can ship anytime.
- Fix: Use `format!("batch rejected at block {b}: {reason:?}")`, and add a report for `BatchSkippedStale`. Reports are not part of the output Merkle tree, so this is safe. Include inclusion_block and sender in the :46 panic, and prev/current blocks in the fold asserts.

### B14. Comments that narrate history or repeat the same rationale several times

`nit · unverified-nit · -40 lines · S · docs-only`  
Where: `sequencer-core/src/scheduler/mod.rs:1210-1218`, `sequencer-core/src/scheduler/mod.rs:1544-1548`, `sequencer-core/src/scheduler/mod.rs:1605-1606`, `sequencer-core/src/scheduler/mod.rs:193-195`, `sequencer-core/src/protocol.rs:20-24`

- Claim: Representative examples. History narration: 'Nothing exercised both until now' (1215), 'Previously tested only alongside an ACCEPTED batch' (1547), 'only the head case was tested' (1605), '`pub` (not `pub(super)`) because ... is now a separate crate' (194-195). The I1 section header at 1210 sits above five unrelated tests (1220-1376). 'The address is identity, not timing' appears four times (protocol.rs:20-24, 99-101, 344-347, safe_accepted_batches.rs:110-112). fold.rs explains its fail-loud rationale three times (19-20, 79-85, 140-143). protocol.rs:33-36 explains why a Result is better than a panic.
- Fix: Delete the history narration, move the I1 header to sit just above the duality helpers (1378), and keep each rationale once at its owning site.

### B15. Spec ties the scheduler's count to the egress snapshot/WS contract; freeze that part

`nit · partially · -8 lines · S · docs-only`  
Where: `docs/protocol/scheduler-semantics.md:177-185`, `openapi.yaml:375`

- Claim: Only spec lines 183-185 (3 lines) tie the count to HTTP snapshots and WS claims. Lines 177-181 describe catch-up and recovery, which the recovery tool still needs. The API field is HistoryInfo.baseline.next_batch_nonce (sequencer-core/src/history_api.rs:29,36; openapi.yaml:375), which belongs to the egress/history area, not the scheduler.
- Fix: Don't polish it. When restructuring (B11), replace it with a one-line pointer to application-history.md. Leave the fold API (fold_replay, resume_at, enqueue_direct, drain_covered_at, finish) as it is: it is already the core the standalone tool needs.

### BS1. Nothing checks that the guest's compiled sequencer_address equals the sequencer's batch_submitter_address, and the spec never says where it comes from

`medium · skeptic-added · +5 lines · S · docs-only`  
Where: `examples/app-core/src/application/wallet.rs:73`, `examples/app-core/src/application/wallet.rs:85`, `examples/canonical-app/src/bin/canonical-app-devnet.rs:4`, `sequencer/src/commands/setup/mod.rs:580`, `sequencer/src/commands/setup/mod.rs:591`

- Claim: Classification is entirely sender == sequencer_address. The guest takes that address from a per-network constant compiled into app-core (wallet.rs:73,85, used by the canonical-app bins). The sequencer, its frontier predicate and the recovery fold use identity.batch_submitter_address instead (setup/mod.rs:580,591). No production code compares the two: git grep for sequencer_address in sequencer/src finds only a doc comment at setup/mod.rs:473. If they differ, the guest files every sequencer batch as a direct input, so no user op executes canonically. Meanwhile the frontier and the fold, which both use the identity address, agree with each other and report acceptance. The result is total silent divergence that only the watchdog's state compare can catch. The spec states the rule (26-29) but never says the address is deployment data compiled into the machine that must equal the submitter key. A porter or operator has nothing to check against.
- Fix: First (docs-only, S): add one spec sentence saying sequencer_address is deployment data compiled into the canonical machine and must equal the sequencer's batch_submitter_address, and put the same item in the deployment/setup checklist. Optional, only if the cost is acceptable: have the guest's 'state' (or a new 'config') inspect report its SchedulerConfig so that setup can refuse on a mismatch. That is a one-time check at setup and adds no runtime machinery. The guest change would ride with the next canonical-machine rebuild.
- Decision: Is a docs and checklist rule enough, or should setup cross-check against an inspect-reported guest config (a guest change that would ride with the next machine rebuild)?

Keep (load-bearing):

- fold_replay's always-on assert! contract checks (sequencer-core/src/scheduler/fold.rs:88-150): run once per recovery, so negligible cost; they turn a dropped direct, which would silently produce a wrong S', into a crash. Extend them (B3), don't remove them.
- fold_replay's pure, iterator-based API, the trusted checkpoint nonce, and the Scheduler fold hooks resume_at/enqueue_direct/drain_covered_at/finish/queued_direct_len (scheduler/mod.rs:122-191): this is the core the planned standalone recovery tool needs, and it drives the canonical source instead of a fourth copy.
- Gate order, with stale and rejected batches never consuming the nonce (scheduler/mod.rs:241-274): cascading invalidation and same-nonce rebuilds depend on it.
- Preflighting nonce exhaustion before any frame mutates state (scheduler/mod.rs:276-278, 289-293): no partially mutated state can exist before the panic.
- Classification by msg_sender, never by decoding (scheduler/mod.rs:214): the payload stays opaque, and a direct can never be misread as a batch.
- The censorship backstop running before classification on every input (scheduler/mod.rs:210-212): the guarantee that the sequencer can't freeze directs. I9's effect-equality argument relies on it being a FIFO prefix of frame 0's drain.
- saturating_sub in age_exceeds (protocol.rs:395-397): keeps the consensus path a total function. Don't turn it into a panic in the guest, even where gate d makes the case unreachable.
- The I1 duality test (scheduler/mod.rs:1401-1540): the only mechanical check of I1. Keep it; B2 just turns its exception block into ordinary agree cases.
- The deliberate inline nonce advance in populate_safe_accepted_batches (sequencer/src/storage/safe_accepted_batches.rs:121-130): it interleaves with the content-identity check and the divergence freeze.
- Graceful inspect errors in the guest (examples/canonical-app/src/scheduler/mod.rs:59-73): a public read-only query must never halt the canonical machine.
- sequencer_address as deployment data rather than a library constant (scheduler/mod.rs:21-25): N16 should give max_wait_blocks the same shape.
- ProtocolTiming::try_new's cross-field validation (protocol.rs:143-183): keep the validation; B6 removes only the re-validation that public fields make necessary.

## C. Fee and numeric stack

Verdict: This area has two parts of very different weight. The consensus core is about 90 lines in sequencer-core/src/fee.rs (fee_to_linear, fee_to_linear_fixed, fixed_mul) plus the 119-line build.rs table generator. Both are recomputed by the guest and by Bart's C++ engine, so their exactness is mandatory. The other ~1,200 lines are sequencer-local: the Uniswap TWAP worker, the oracle math, the log-additive batch_policy view and admin knobs. Nothing re-derives any of their outputs. The frame fee is published and the scheduler only consumes it (scheduler/mod.rs:330-341, wallet.rs:244,264). The worker lifecycle is proportionate: it validates the source at setup, builds the runtime source without RPC, retains the last price on transient failures and absorbs contention. The numeric rigor is not proportionate. The TickMath port is already gone (7cfb474, uniswap.rs:247 is f64), but its doctrine remains. An f64 quote goes through checked U256 ceil-division, which adds up to 4x overcharge at sub-gwei L1 gas. A U512 binary search then encodes it, guarded by a 'fail rather than undercharge' error that the view's saturation contradicts downstream. The consensus crate also exports three inverse functions whose only callers are tests, so a porter cannot tell which parts are contract. The biggest open issue is a correctness question: a frame fee above MAX_EXPONENT panics canonical execution instead of being a structural batch rejection, and no C++ port can reproduce that. The batch-fee hoist is not designed anywhere in the repo; AGENTS.md:218 only mentions it. So batch_policy should be frozen, not polished. Keep: the fee_to_linear spec text and fixed_mul truncation argument, frame-fee immutability, the no-RPC runtime source, exact "OLD" revert decoding, and the queries.rs clamp that keeps the sequencer from emitting an out-of-range fee.


### C1. Out-of-range frame fee_price panics canonical execution; the behavior is unspecified for ports

`low · confirmed · +15 lines · S · BC · wire`  
Where: `sequencer-core/src/fee.rs:68-71`, `sequencer-core/src/scheduler/mod.rs:100-105`, `sequencer-core/src/scheduler/mod.rs:229-262`, `sequencer-core/src/scheduler/mod.rs:330-341`, `examples/app-core/src/application/wallet.rs:244`

- Claim: As stated. The trigger needs two things: a frame with fee_price > MAX_EXPONENT (~17.1k), and an included op with a valid nonce and max_fee >= fee_price. Ingress never bounds max_fee, so anyone can sign 65535 and leave such an op waiting. The sequencer's own inclusion path would panic natively first (wallet.rs:244 runs on the host too), so the realistic source is a bug in the batch encoder or persistence, or a compromised key.
- Fix: Decide the rule with Bart before any Track 5 work, since a lookup table has the same bound question. (a) Add one structural rejection (e.g. BatchRejectReason::FeeAboveMax, checked in batch_reject_reason_for_block over all frames, ~10 lines) and document it in scheduler-semantics.md and the C header. Or (b) declare the panic part of the contract (guest halts) and say so in application-engine.h. Ship it as its own behavior patch.
- Decision: Should a sequencer-authored out-of-range frame fee be a graceful structural batch rejection (like DecodeFailed) or an impossible state that halts the guest? Bart's engine must implement the same rule.
- Independent trace: confirmed. The code path reproduces exactly as claimed, but the framing needs three corrections.

### C2. Oracle price pipeline: an integer ceil in linear units/gas overcharges up to 4x at sub-gwei gas; collapse it to one f64 log expression

`medium · partially · -120 lines · M · BC`  
Where: `sequencer/src/l1/fee_oracle/math.rs:4`, `sequencer/src/l1/fee_oracle/math.rs:9-57`, `sequencer/src/l1/fee_oracle/uniswap.rs:152-176`, `sequencer/src/l1/fee_oracle/uniswap.rs:237-252`, `sequencer/src/l1/fee_oracle/worker.rs:38-39`

- Claim: The whole-unit ceil at math.rs:33-40 overcharges by at most about 2x, when the true value is just above 1 unit/gas (e.g. 1.25 becomes 2 at 0.5 gwei for USDC at $2500, which is 1.6x). The '4x at 0.1 gwei' example is not caused by the ceil. It comes from the exponent-0 floor: fee_from_linear_ceil returns 0 for anything <= 1 (fee.rs:136-138), and the column has CHECK log_gas_price >= 0 (schema:573). The proposed f64 formula with clamp(...,0,...) keeps that floor, so it would not fix the 4x (see CS1). The 'contradiction' with the view's saturation cannot happen in practice: FatalMath needs about 1e53 units/gas, and Overflow needs gas above about 1e20 wei. That strengthens the case for deleting it, but it is not a live inconsistency. The simplification itself holds (MathError, FatalMath and the U256 quote plumbing go), as long as the 'never undercharge' doc is dropped explicitly: an f64 ceil can land one exponent (0.78%) low near boundaries, which the 10x slack absorbs.
- Fix: Compute log_gas_price = clamp(ceil((ln(base+priority) + directed_tick*ln(1.0001)) / ln(129/128)), 0, MAX_EXPONENT) in f64, with base+priority = 0 mapping to 0. TokenPriceSource returns the mean tick. While there, destructure observe's two cumulatives with a checked pattern that returns Provider, instead of indexing at uniswap.rs:159-164. Delete compute_x_units_per_gas, encode_log_gas_price, MathError, FeeOracleError::FatalMath and the U256 quote plumbing. Keep the tick range check, and log the f64 linear value for operators. Tests switch to tolerance comparisons. If deferred, the stopgap is the 3-line comment fix at math.rs:4.

### C3. The consensus crate exports sequencer-local inverses whose only callers are tests

`medium · partially · -250 lines · S · public-rust-api`  
Where: `sequencer-core/src/fee.rs:15-16`, `sequencer-core/src/fee.rs:27-31`, `sequencer-core/src/fee.rs:81-127`, `sequencer-core/src/fee.rs:174-219`, `sequencer-core/src/fee.rs:104-115`

- Claim: The dead code is real. fee_from_linear (fee.rs:81-127) is called only by tests (fee.rs:292,364-386; math.rs:119). log_fee_ratio (fee.rs:174-219) is called only by set_alpha. set_alpha (admin.rs:15-34) is called only by tests (inclusion_lane/tests.rs:1284,1850; admin.rs:115). FEE_BASE_NUM/DENOM are unused, and build.rs:71 hardcodes 129<<57. The same lower-bound binary search is copied three times. The 'porter cannot tell which parts are contract' reason is overstated: fee.rs:10-11 and c-application-binding.md:41-43 already pin the contract to fee_to_linear. Deleting set_alpha also removes the only typed writer listed in the invariants register writer table (docs/invariants.md:90, 'admin'), so update that row and schema:567, and replace the two lane-test calls with raw SQL fixtures. Counted alone, without C2's deletions, the delta is about -200, not -250.
- Fix: Delete fee_from_linear, log_fee_ratio, FEE_BASE_NUM/DENOM and set_alpha, along with their tests. Keep the 296 = log(10) derivation as the schema comment it already is. After C2, delete fee_from_linear_ceil and FeeEncodeError too. Move suggested_signing_max_fee and its slack constant next to FeeQuote (api.rs:130-147). fee.rs is then exactly the consensus core that Track 5 would replace.

### C4. FEE2: every 12s poll opens a FULL-sync writer and commits an unchanged price

`nit · partially · -3 lines · S · BC`  
Where: `sequencer/src/l1/fee_oracle/worker.rs:133-134`, `sequencer/src/l1/fee_oracle/worker.rs:146-162`, `sequencer/src/storage/fee_oracle.rs:30-41`, `sequencer/src/storage/open.rs:166-173`, `sequencer/src/l1/fee_oracle/worker.rs:386-419`

- Claim: The mechanics are right: a new writer connection and an unconditional UPDATE on every poll (worker.rs:151-154, storage/fee_oracle.rs:30-41, open.rs:166-173). By the reviewer's own numbers the cost is negligible (under 0.1%), and the proposal pre-empts an open register decision. 'Fee-observation visibility' (register.md:175-181) keeps 'operator SQL inspection suffices' as an option, and that option needs exactly the per-poll 'last successful observation' semantics. A last-change stamp would lose liveness information. invariants.md:91 also records 'stamps on every successful refresh' as the writer contract. This is not a standalone finding; it belongs to the register entry. The one concrete test defect: worker.rs:419 asserts the stamp with >=, which passes even if no second write happened.
- Fix: Write only when the exponent changes. The stamp then means 'last change', or it is dropped once the register entry is decided. Update invariants.md:91 and worker.rs:133-134. If staleness visibility matters, put the retained price's age in the transient warn (C8) rather than in a per-poll write.
- Decision: Should log_gas_price_updated_at_ms become a last-change stamp or be dropped? The register entry 'Fee-observation visibility' owns this.

### C5. PriceSourceError is classified twice and passes through a tuple and an extra enum on the setup path

`low · confirmed · -35 lines · S`  
Where: `sequencer/src/l1/fee_oracle/uniswap.rs:221-230`, `sequencer/src/l1/fee_oracle/worker.rs:230-243`, `sequencer/src/l1/fee_oracle/bootstrap.rs:15-46`, `sequencer/src/commands/setup/mod.rs:164-178`

- Claim: As stated. The same transient/misconfig split is coded at uniswap.rs:224-230 and worker.rs:230-243. The setup path goes PriceSourceError, then (bool,String), then UniswapConnectError (bootstrap.rs:17-23,37-44), then BootstrapError (setup/mod.rs:170-178). The single-field struct variant is matched with '..' (setup/mod.rs:171). The net delta is closer to -25 once is_transient() is added.
- Fix: Add PriceSourceError::is_transient(&self) and use it in both places. connect_uniswap returns PriceSourceError, with create_provider failures mapped at the call site, and setup maps the error once. Delete UniswapConnectError, bootstrap_price_source_error and its test. This does not merge BootstrapError variants.

### C6. Freeze the log-additive batch_policy design until the batch-fee hoist (~250 lines)

`low · partially · -200 lines · M · BC · db-schema`  
Where: `sequencer/src/storage/migrations/0001_schema.sql:535-625`, `sequencer/src/storage/queries.rs:139-177`, `sequencer/src/storage/mod.rs:297-303`, `sequencer/src/storage/mod.rs:375-384`, `sequencer/src/storage/admin.rs:37-135`

- Claim: Freezing batch_policy is reasonable, but for a different reason than given. coordination-tracks.md:16 records a pending decision that 'may make log-space fees defunct', and AGENTS.md:218 defers the batch-fee hoist. Neither is the stated future direction (a shared cockroach-recovery tool plus a new client sync). This is 'frozen pending a fee decision', not 'superseded-by-direction', and its -200 lines and db-schema change are speculative while no design exists. The claim that the CHECK log_gas_price >= 0 floor is 'a log-space artifact the hoist removes' is wrong. log_gas_price is only summed with about +1356 of constants (schema:617-621), so a negative exponent stays representable. The floor comes from the unsigned u16 oracle output and the CHECK, and it can be fixed now (CS1).
- Fix: Don't polish this now. When the hoist lands: compute a linear batch fee in app code, turn the batch target into a plain byte knob, and drop the view, the clamps, set_alpha's coupling and batch_size_target_bytes. Revisit the /fee quote shape at the same time.
- Decision: The batch-fee hoist design itself, including whether the user signs a linear or a log-space batch fee. It decides C6 and whether Track 5 is ever built.

### C7. Fee docs: a wrong view comment, a stale name, and the 10x-slack fact repeated in 6+ places

`low · confirmed · -30 lines · S · docs-only`  
Where: `sequencer/src/storage/migrations/0001_schema.sql:618`, `sequencer-core/src/batch.rs:19-21`, `sequencer-core/src/batch.rs:23-38`, `sequencer/src/storage/migrations/0001_schema.sql:537-552`, `sequencer/src/storage/migrations/0001_schema.sql:577-578`

- Claim: As stated, with two small fixes. The wrong comment is at schema:619, not 618: the sum adds log_user_op_bytes, so the result is a per-op fee, and the wallet charges fee_to_linear(frame_fee) per op. fee.rs:226-227 is not a redundant restatement: it tells the 1.5x signing slack apart from log_slack and should stay. The other slack restatements (schema:545-546,578; batch.rs:34-35; math.rs:20-21; uniswap.rs:234; threat-model:24) are real duplication.
- Fix: Keep the fee derivation in one place, the schema header, and replace batch.rs:23-38 with one line pointing to it. Fix the two errors, drop the TickMath reference, and cut the restating doc lines.

### C8. Oracle logs say little during outages and may print the RPC URL every 12s

`nit · partially · +5 lines · S`  
Where: `sequencer/src/l1/fee_oracle/worker.rs:164-184`, `sequencer/src/l1/fee_oracle/worker.rs:201-205`, `sequencer/src/l1/fee_oracle/worker.rs:206-216`, `sequencer/src/l1/fee_oracle/uniswap.rs:90-110`, `sequencer/src/l1/fee_oracle/uniswap.rs:333-340`

- Claim: Two parts hold. A change is logged twice: info at worker.rs:164-173 and debug at worker.rs:202-204. The changed= field is constant in each branch (worker.rs:171,181). The step-label point applies only at setup: at runtime only observe produces PriceSourceError::Provider (uniswap.rs:158), and gas failures carry their own string (eip1559.rs:40,47). Rate limiting and tracking failure counts or age adds state, which a cosmetic problem does not justify: about 300 warns per hour during a pool-only outage is acceptable. URL exposure is plausible, since reqwest transport errors print the URL and estimate_fees stringifies them too. But that is the class already covered by the open register entry 'Startup logs the full RPC URL', whose Next step already says to check diagnostic paths (register.md:133-135). It belongs in that entry's triage, not in a new finding.
- Fix: Log a change once. Add retained-price age and failure count to the warn and rate-limit it. Add a step label to Provider. Route provider errors through whatever endpoint redaction the RPC-URL register entry settles on.

### C9. Ceremony around the 'OLD' revert classification

`nit · unverified-nit · -15 lines · S`  
Where: `sequencer/src/l1/fee_oracle/uniswap.rs:191-219`, `sequencer/src/l1/fee_oracle/uniswap.rs:296-304`

- Claim: is_old_observation_reason wraps reason == "OLD" in its own function with its own doc comment, and has a 5-assert test of string equality. The two doc comments above it repeat each other.
- Fix: Inline Some(r) if r.reason == "OLD", keep one doc comment, and drop the equality test.

### CS1. Oracle floors log_gas_price at exponent 0 (1 smallest unit per gas), overcharging 6-decimal fee tokens by 1/x at low gas

`medium · skeptic-added · +10 lines · S · BC · db-schema`  
Where: `sequencer/src/storage/migrations/0001_schema.sql:573`, `sequencer/src/l1/fee_oracle/math.rs:33-40`, `sequencer-core/src/fee.rs:136-138`, `sequencer/src/storage/fee_oracle.rs:12,30`, `sequencer/src/commands/config.rs:120`

- Claim: compute_x_units_per_gas rounds any nonzero quote up to at least 1 unit/gas, and fee_from_linear_ceil maps values <= 1 to exponent 0. log_gas_price is a u16 end to end, and the column has CHECK log_gas_price >= 0. With USDC (the wallet prototype's token, schema:543) and ETH at $2500, the true cost is 2.5 x gas_gwei units/gas. Below 0.4 gwei the oracle therefore reports 1 unit/gas, an overcharge of 1/x: 4x at 0.1 gwei, 10x at 0.04 gwei. The 10x safety margin is applied a second time, by accident. The floor is not inherent to log space: log_gas_price is only added to about +1356 of constant exponents in the view, so values down to about -1356 still give a valid non-negative frame fee.
- Fix: Let the oracle produce a signed exponent: i16 in RefreshResult, set_log_gas_price, log_gas_price() and FeeOracleMode::Fixed. Relax the CHECK to a lower bound that keeps the derived fee >= 0, e.g. log_gas_price + log_slack + log_one_plus_alpha + log_delta + log_user_op_bytes >= 0. Do it together with C2: the f64 log formula then clamps at that bound instead of 0. Ship it as its own behavior patch. This is a schema change, so ask first.
- Decision: Should the oracle price below 1 smallest unit per L1 gas (relax the schema CHECK and make the exponent signed), or is the fee token's smallest unit an accepted floor? Either way, record the trade-off in the schema header.

Keep (load-bearing):

- fee_to_linear's contract text and the fixed_mul truncation argument (sequencer-core/src/fee.rs:44-59, 248-251): this is the cross-implementation spec Bart's C++ engine ports (c-application-binding.md:39-46), and the doc is the spec until a Track 5 table exists.
- build.rs table generation plus the MAX_EXPONENT assert (sequencer-core/build.rs:42-80, fee.rs:68-71): this is consensus data and its bound fails loud. Track 5 only turns build.rs into a verifier.
- query_batch_policy's asymmetric clamp and panic (sequencer/src/storage/queries.rs:146-176): the clamp is why the sequencer never emits a frame fee above MAX_EXPONENT, which keeps C1 unreachable today.
- Frame fee sampled at frame open and immutable for the frame (AGENTS.md:218, storage/mod.rs:345-372): this is what lets the oracle's math be free. The scheduler only consumes the published exponent.
- Setup-time connect and validate plus no-RPC runtime construction (bootstrap.rs:28-67, uniswap.rs:83-150): this keeps run independent of the pool, and it is what fixed FEE1.
- Exact decoding of the Error(string) 'OLD' revert rather than substring matching (uniswap.rs:202-219): this is correct classification of an availability failure. Only the wrapper ceremony should go (C9).
- is_sqlite_contention absorption and the transient-retention loop (worker.rs:62-72, 195-227): these match the accepted oracle-outage policy in threat-model README:24 and avoid restart storms.
- FeeOracle's process_lock with spawn_blocking_with_lock (worker.rs:92-95, 146-162): it keeps data-directory exclusivity if the awaiting task is cancelled.
- SUGGESTED_MAX_FEE_SLACK as a ceil (53) rather than the nearest value (fee.rs:221-239, test 337-347): nearest rounding would understate the 1.5x margin that lets quotes survive a frame rotation.
- Pinned Alloy estimator constants test (sequencer/src/l1/eip1559.rs:65-76): it guards the external policy that both the poster and the oracle depend on.
- Schema CHECK log_base_gas - log_alpha - log_delta < log_max_batch_bytes (0001_schema.sql:590): it enforces the batch-size bound until the hoist replaces the design.

## D. Ingress and inclusion lane (hot path)

Verdict: The ingress/lane area takes a signed user op from POST /tx, checks only size and signature (api.rs:131-156), and hands it to a single blocking lane over a bounded channel. The lane executes bounded chunks, commits each accepted chunk with FULL sync before acking (mod.rs:183-206, 311-335), and on a separate time-gated cadence does L1 reconciliation and batch close with a snapshot. I traced the hot path end to end and found no correctness bug and no per-op clone worth removing. Ops move by value and execute by reference. The real latency sources are the per-chunk fsync, the inline WAL checkpoint, and the occasional batch-close or reconciliation stall, and those are already in the register. Size is mostly proportionate. The lane is about 500 lines of essential concurrency and durability code. storage/ingress.rs (1668 lines) is about 900 lines of tests plus about 770 production lines, but roughly 240 of those are #[cfg(test)] fixtures mixed into production code. Some of those fixtures only test themselves, and a few public helpers are dead. A lean file keeps about 530 lines. The biggest real problem is diagnostic: when the lane dies, the error that gets logged and written to the black-box record drops its cause, so an AppError reason or a rusqlite detail never reaches the operator. After that come one redundant piece of in-memory state (the drain cursor) and a set of stale or untrue comments. The good parts must stay: ack only after a durable commit, bounded fast turns so rejected floods cannot starve reconciliation, WriteHead counter checks before the transaction, the re-derivation and receipt assert at frame rotation, hash-at-seal, file-first snapshots, fixed public 500 messages, and the documented no-nonce-precheck admission policy. N10 is already implemented: frames rotate on the 5-block clock whether or not there are direct inputs. N24 (the listener split) is cheap and should go ahead as a small PR. The egress snapshot-lease machinery in http.rs falls under the future direction and should be frozen, not polished.


### D1. Fatal lane errors lose their cause in the exit log and the black-box record

`medium · partially · +10 lines · S · BC`  
Where: `sequencer/src/ingress/inclusion_lane/error.rs:17-44`, `sequencer/src/commands/error.rs:515-516`, `sequencer/src/commands/error.rs:572-573`, `sequencer/src/harness.rs:129`, `sequencer/src/commands/mod.rs:65`

- Claim: The core claim holds. ExecuteUserOp, ExecuteDirectInput, CatchUp, Snapshot and Gc display fixed strings (inclusion_lane/error.rs:17-21, 29-44). WorkerStop::Source is `#[error("{0}")]` (commands/error.rs:572-573), and every log sink formats Display only, so the AppError reason, the rusqlite detail, and even CatchUpError's own detail (e.g. the SnapshotExecutionCountMismatch numbers) never reach the operator. The sink list is wrong, though. A terminal lane error (e.g. AppError::Internal) never reaches harness.rs:129 or the black-box write at commands/mod.rs:65. Runtime::finish aborts first (commands/run/workers.rs:372-375), and abort_terminal logs `error = %error` and calls abort() (runtime/shutdown.rs:16-18). README.md:158 documents this: terminal runtime faults abort without settlement. Non-terminal lane errors reach harness.rs:129 (Unclassified) or workers.rs:460 during the drain. A sink-side chain renderer would therefore have to cover abort_terminal, harness.rs:129 and workers.rs:460, not commands/mod.rs:65. Inlining `: {source}` in the five #[error] strings, matching CatchUpError (error.rs:105, 110) and LoadFromDump (error.rs:41), covers every sink with no other change and is the smaller fix.
- Fix: Pick one fix. Either inline `{source}` into the five InclusionLaneError #[error] strings, matching CatchUpError, or render the full source chain once at the two top-level sinks (harness.rs:129 and commands/mod.rs:65). The second fixes every worker at once. Either way, the operator sees e.g. 'inclusion lane: user op execution failed: <AppError reason>'.
- Decision: Choose between per-variant inlining and a chain renderer at the sink. The renderer covers the other workers too, but touches files outside this area.

### D2. storage/ingress.rs mixes about 240 lines of test-only fixtures, some self-testing, into production code; two helpers are dead

`medium · confirmed · -110 lines · M`  
Where: `sequencer/src/storage/ingress.rs:93-153`, `sequencer/src/storage/ingress.rs:216-286`, `sequencer/src/storage/ingress.rs:323-405`, `sequencer/src/storage/ingress.rs:440-442`, `sequencer/src/storage/ingress.rs:635-702`

- Claim: Confirmed as stated. About 237 #[cfg(test)] lines sit in the production half of storage/ingress.rs: 93-114, 115-145, 147-153, 216-286, 323-348, 365-379, 381-405, 635-650 and 679-702. fill_safe_inputs (216-286) is used only by the half-open-interval test at l1_inputs.rs:466-508, so that test largely tests its own fixture. It could use the existing production read safe_inputs_in_block_range (l1_inputs.rs:120) instead of a new SELECT. open_recovery_tip's guard (147-153) is exercised only by ingress.rs:1185-1188. WriteHead::open_frame_has_user_ops (storage/mod.rs:342-344) has no callers anywhere in the repo. Storage::batch_policy (ingress.rs:440-442) has only test callers (admin.rs:56/80/107, fee_oracle.rs:70, ingress.rs:1195+). append_user_ops_chunk has the two test users the reviewer names. The integration_tests module compiles in-crate (lib.rs:33-35), so cfg(test) items are visible to it.
- Fix: Delete fill_safe_inputs and replace the test read-back with a 5-line SELECT. Delete open_recovery_tip and its assertion, since the guard belongs at the external boundary that already exists. Delete open_frame_has_user_ops. Make batch_policy cfg(test) or move it into test_helpers. Port the two physical-only fixture users to the production append. Move the remaining fixtures (initialize_open_state, ensure_open_tip, close_frame_only/close_frame_derived_in, close_frame_and_batch) into storage/test_helpers.rs. The production file drops to about 530 lines and contains only lane writes, the in-transaction helpers, and the two public reads. Do this as a pure refactor patch.

### D4. Redundant checks and parameters around snapshot restore and batch close

`low · confirmed · -25 lines · S`  
Where: `sequencer/src/ingress/inclusion_lane/mod.rs:102-109`, `sequencer/src/ingress/inclusion_lane/catch_up.rs:16-31`, `sequencer/src/ingress/inclusion_lane/catch_up.rs:47-52`, `sequencer/src/storage/ingress.rs:409-420`, `sequencer/src/ingress/inclusion_lane/snapshot.rs:48-57`

- Claim: Confirmed with one correction. The count check at mod.rs:102-109 repeats catch_up.rs:47-52 on the same values: checkpoint.executed_input_count is passed straight through run_forever, run_catch_up and catch_up_application as `start`, and nothing touches the app in between. CatchUpSnapshot (catch_up.rs:16-31) only re-wraps latest_snapshot's prefix and count. close_frame_and_batch_with_snapshot's batch_index parameter only feeds an assert, which is tautological in production (snapshot.rs:48, 56; ingress.rs:417-420). The correction is that it has one production caller but six test callers (snapshot_dumps.rs:504, ingress.rs:838, integration_tests/snapshot_endpoints.rs:125/156/197, recovery_compatibility.rs:124, egress/historical/tests.rs:324), which must change with it. Expect about -15 to -20 lines rather than -25.
- Fix: Keep the check in catch_up, which is the place tests exercise, and drop the copy in start(). Return the storage snapshot row from catch_up_snapshot, or inline it. Remove the batch_index parameter and use head.batch_index inside storage.

### D5. Stale or untrue comments in the hot-path files

`low · confirmed · -10 lines · S · docs-only`  
Where: `sequencer/src/storage/mutations.rs:22-24`, `sequencer/src/storage/ingress.rs:603-608`, `sequencer/src/storage/queries.rs:178-183`, `sequencer/src/clock.rs:6-8`, `sequencer/src/ingress/inclusion_lane/types.rs:33-37`

- Claim: Items 1, 2, 3 and 5 are confirmed as stated. mutations.rs:22-24 is stale: production passes Some(0) via open_fresh_tip_in_tx (ingress.rs:491) and open_recovery_tip_in_tx (ingress.rs:526). The close_frame_in doc (ingress.rs:603-608) is wrong because close_frame_derived_in does not call close_frame_in. decode_l2_tx_row is #[cfg(test)] (queries.rs:178-184). types.rs:36 is wrong because queue-full is ApiError::overloaded at api.rs:266 and Unavailable maps only to 503 (http.rs:96, 113-114). For item 4 the conclusion is right but the evidence is weak: the cited sites (mod.rs:363, api.rs:261, snapshot.rs:70) need a SystemTime or nanos, not 'now in Unix-ms'. The real contradiction is that production storage has its own duplicate Unix-ms helper, storage/convert.rs:66 now_unix_ms(), used at ingress.rs:452 and lifecycle.rs:126, which bypasses clock::unix_now_ms. No test clock exists anywhere.
- Fix: Correct or delete each one. For clock.rs, say what it is (a saturating Unix-ms helper) and drop the test-clock promise.

### D6. Verbose comments that restate the code or repeat neighbouring docs

`nit · unverified-nit · -40 lines · S · docs-only`  
Where: `sequencer/src/storage/ingress.rs:116-136`, `sequencer/src/storage/ingress.rs:4-11`, `sequencer/src/ingress/inclusion_lane/mod.rs:69-81`, `sequencer/src/ingress/inclusion_lane/mod.rs:130-135`, `sequencer/src/ingress/inclusion_lane/mod.rs:155-159`

- Claim: Representative examples: a 21-line doc on an 8-line test-only fn, mostly describing production behavior that lives elsewhere (ingress.rs:116-136); a module doc that says 'live here too' twice (ingress.rs:4-11); InclusionLane::start's 13-line doc mixing three topics (mod.rs:69-81); an inline comment repeating that doc (mod.rs:130-135); a batch-close comment restating snapshot.rs:39-40 and I7 (mod.rs:155-159); a doc explaining that &Transaction coerces to &Connection (mutations.rs:146-151). The module doc also has a single line over 150 columns (mod.rs:14).
- Fix: Cut each to one or two lines that state the non-obvious reason, or link the invariant (I7, I17). Rewrap mod.rs:14.

### D8. Duplicated shutdown gate and a redundant constructor in api.rs

`nit · unverified-nit · -15 lines · S`  
Where: `sequencer/src/ingress/api.rs:51-80`, `sequencer/src/ingress/api.rs:92-108`

- Claim: SubmitState and ReadState each carry an identical reject_if_shutting_down. SubmitState also has a 4-argument new() even though all its fields are pub.
- Fix: Make one free fn reject_if_shutting_down(&RuntimeScope). Pick either pub fields or a constructor. Fold this into the N24 split PR, which touches this wiring anyway.

### DS1. A graceful shutdown racing a frontier check or rotation surfaces as ChannelClosed and exits 1 instead of 0

`low · skeptic-added · +4 lines · S · BC`  
Where: `sequencer/src/ingress/inclusion_lane/mod.rs:143-150`, `sequencer/src/ingress/inclusion_lane/mod.rs:439-443`, `sequencer/src/http.rs:297-352`, `sequencer/src/commands/run/workers.rs:454-470`, `sequencer/src/commands/error.rs:179-185`

- Claim: The lane checks the shutdown flag only at the top of its loop (mod.rs:144), then runs maybe_advance_safe_frontier, which can rotate the frame and run GC (mod.rs:254-265), before try_recv. The only lane senders are owned by the HTTP router (SubmitState and HealthState, http.rs:305-315), and they drop as soon as graceful HTTP shutdown completes. If shutdown is requested after the flag check and the server finishes first, try_recv sees Disconnected with attempted == 0 and the lane returns InclusionLaneError::ChannelClosed (mod.rs:440-442) instead of Ok(()). The drain treats that as an error (workers.rs:456-463). It is non-terminal, so it is classified Unclassified (error.rs:179-185) and a clean SIGTERM exits 1 with a 'component errored during runtime drain' warning.
- Fix: In the Disconnected arm, or where run_fast_turn's error is handled, treat ChannelClosed as a graceful stop when self.shutdown.is_shutdown_requested(): reject anything pending and return Ok(()). That is about 3-5 lines and adds no new machinery.

Refuted:

- D3. The lane's in-memory drain cursor duplicates a value storage already re-derives and asserts on every frame rotation. The proposal weakens a fail-loud cross-check that protects against silent scheduler divergence (I6 drain completeness, 'impossible states fail loud').
- D7. Egress snapshot-lease release supervisor lives in the shared http.rs; freeze it and move it with egress at the split. The 'superseded-by-direction' premise fails: the same lease machinery serves the watchdog and operator recovery endpoints, which the direction keeps and probably relies on.

Keep (load-bearing):

- Ack only after the FULL-synced chunk commit (mod.rs:311-335, ingress.rs:297-321). The commit is the durability authority behind a 200 (I20).
- Bounded fast turn plus time-gated frontier check, with TurnOutcome's three states (mod.rs:7-18, 337-356, 423-453). This stops a continuously nonempty, all-rejected queue from starving reconciliation.
- WriteHead counters are checked for overflow before the transaction, and the cache is updated only after commit (ingress.rs:305-320; I17).
- Range re-derivation and execution-receipt equality assert at frame rotation (ingress.rs:616-623, mutations.rs:169-173, 210-262). This enforces I6/I19/I20 and runs only once per safe-head tick, not per op.
- Hash-at-seal inside the seal transaction (ingress.rs:582-590). The content-identity check needs the hash atomically with the seal.
- File-first dump, then atomic seal plus snapshot registration (snapshot.rs:39-60, ingress.rs:409-438; I7).
- Unique dump-dir naming with nonce, nanos and a counter (snapshot.rs:62-75). Nonces are reused across recovery cascades.
- is_terminal_invariant classification and its tests (error.rs:52-138, 140-237). It drives the exit codes the supervisor relies on.
- storage_task classifies errors inside the blocking task and aborts on persistent faults (http.rs:213-267). Cancelling the HTTP request must not discard a persistent fault.
- No nonce/max_fee precheck at ingress, with a named revisit trigger (api.rs:18-23). A precheck would add a SQLite read to every honest submit.
- Fixed public error messages: no AppError, serde or rusqlite detail in 4xx/5xx bodies (mod.rs:392-401, api.rs:230-248).
- next_user_nonce's valid_batches filter (ingress.rs:43-47, 73-91). Cascades reuse nonces from invalidated rows.
- The asymmetric clamp/fail-loud in query_batch_policy (queries.rs:146-171). Operator-writable bounds are clamped; contract-impossible ones panic.
- catch_up_application_paged's page-size parameter (catch_up.rs:41-46). It is the test seam for page-boundary coverage.
- SnapshotReleaseDrain (http.rs:150-211). It is correct and load-bearing for lease release on shutdown, but superseded by direction, so freeze it rather than delete it.

## E. Egress API and client SDK (consumer surface: clients/indexers)

Verdict: Area E is the internal consumer surface. It has five parts: a WebSocket feed that replays and then follows the tip, keyed by a mandatory (era, generation, next_input) claim; /history metadata with standard-recovery compatibility; a pager for the era's raw L1 prefix (/historical-l1-inputs); health probes; and a Rust SDK. The core is sound and proportionate. One DB-backed cursor re-validates identity on every page inside a single deferred read transaction, refusals are typed and returned as 409 before the upgrade, and the base offset K and the generation cuts are implemented and fail loud on gaps. I found no correctness bug in the replay/subscribe handoff or in the history-identity checks, and N11/N12 are done. The future direction does not justify the current size: about 1,600 lines (code, tests and docs) exist only so a client can rebuild a projection across a cockroach rebuild (/historical-l1-inputs, projection-replay.md, the historical_bootstrap integration test, and half of /history), and the shared recovery tool replaces exactly that. About 1,400 more lines (WS wire format, SDK snapshot-then-subscribe bootstrap) should be frozen until the new sync is designed. The real problems besides supersession: the contract is written out in three places (README, openapi.yaml, protocol docs); the feed re-implements http::storage_task's terminal-fault policy with two near-identical error enums; WS error logs drop their SQLite causes; and the SDK has dead surface. Keep the claim identity (I18), per-page revalidation, cuts with the complete-lineage assert, base_executed_input_count, and the canonical reader. That reader is shared with native restart and should move out of storage/egress before the freeze.


### E1. Freeze /historical-l1-inputs and the projection-replay path (~1,600 lines)

`medium · partially · -1600 lines · M · BC · http-api`  
Where: `sequencer/src/egress/api/history.rs:37-46`, `sequencer/src/egress/api/history.rs:101-176`, `sequencer/src/storage/egress/historical.rs:115-260`, `sequencer-core/src/history_api.rs:11-13`, `sequencer-core/src/history_api.rs:56-81`

- Claim: The direction does supersede the raw-prefix pager. Its only consumer is projection replay across a rebuild (projection-replay.md:12-28; the genesis prefix is empty). Freezing it is right. The removal plan is wrong in two places. (a) historical_bootstrap.rs is also the parent module of recovery_compatibility.rs (historical_bootstrap.rs:7, 403 lines, which uses `super::*` at recovery_compatibility.rs:15). That file tests in-era standard-recovery checkpoint compatibility, which E2 says survives. Deleting historical_bootstrap.rs would drop it together with the helpers it reuses (domain/config/execute, historical_bootstrap.rs:53-204). (b) projection-replay.md:56-93 (checkpoint eligibility: seed directs after A, replay (B,C], terminal drain) and the worked boundary at 95-118 specify exactly the client-state reconstruction the shared tool must perform. The main historical_bootstrap test is a ready-made oracle for it. They should move into the tool's design and tests, not be deleted. Only the HTTP route, raw_page_in/first_input_after_block_in, the HistoricalL1Input* wire types, the SDK method and the README/openapi prose are pure deletions.
- Fix: Freeze now: no new features, no polish, and do not implement the Track 3 'Cockroach reader recovery' follow-up (track3 plan:79,86-92). When the shared tool lands, delete the route, raw_page_in/first_input_after_block_in, HistoricalL1Input*, the SDK method, projection-replay.md and historical_bootstrap.rs in one removal patch. Keep historical_end_in only if /history still reports R.
- Decision: Confirm that the shared tool will produce the client's projection from the client's own checkpoint. If any client must still rebuild a projection from raw L1 after a rebuild, this route survives.

### E2. /history: baseline and accepted_checkpoint serve cross-era projection prep; history and compatibility survive

`low · partially · -150 lines · S · BC · http-api`  
Where: `sequencer-core/src/history_api.rs:15-54`, `sequencer/src/storage/egress/historical.rs:41-113`, `sequencer/src/storage/egress/historical.rs:28-38`, `sequencer/src/egress/api/history.rs:84-86`, `README.md:318-390`

- Claim: baseline.{l1_stop_block,l1_end_input_index,next_batch_nonce} and accepted_checkpoint serve only projection replay and manual-recovery checkpoint prep. They appear only in the README, openapi, projection-replay.md, the SDK and tests (git grep), so freezing them is right. The duplicate check is real: api/history.rs:84-86 makes historical.rs:54-58 unreachable through the route, though historical/tests.rs:403 still exercises it. Two corrections. (a) BadRequest-from-storage cannot simply be dropped. The 'from_generation exceeds the current recovery generation' check (historical.rs:59-63) needs the generation read inside the transaction, so after E1 it still needs a storage-side refusal. Only the pager's BadRequests (130-150) and 54-58 go away. (b) The claim that compatibility 'survives the direction' is not established. It exists so a client can reuse a replica checkpoint and resubscribe over WS after standard recovery. That is the claim/feed model E3 freezes, so it should be frozen with E3, not declared a keeper.
- Fix: Freeze. When E1 is removed, reshape /history to {deployment, history, compatibility} and drop the unreachable storage-side era check together with BadRequest-from-storage. No change before then.
- Decision: Is accepted_checkpoint still needed by the watchdog or operators once client recovery goes through the shared tool? The snapshot reviewer owns finalized_dump_in.

### E3. Freeze the WS wire format and SDK snapshot-to-subscribe bootstrap; record their defects as inputs to the new sync

`medium · partially · +0 lines · S · wire`  
Where: `sequencer/src/egress/api/subscribe.rs:1-140`, `sequencer-core/src/broadcast.rs:12-46`, `sequencer-core/src/api.rs:189`, `sdk/rust-client/src/lib.rs:27-34`, `sdk/rust-client/src/lib.rs:177-228`

- Claim: The snapshot-then-subscribe replica bootstrap (lib.rs:177-228) is the shared-state assumption the direction rejects, so freezing the WS wire format and the SDK bootstrap is right. All five listed defects are real: String fields at broadcast.rs:15-45 vs typed fields at history_api.rs:62-71; the casing split documented at openapi.yaml:16-20; the alias at api.rs:189; the raw stream at lib.rs:27; no timeout at lib.rs:222-228. Defect (5) should not be frozen, though. subscribe() can hang forever on a stalled server, while every sibling method is bounded (lib.rs:104, 154, 185). The fix is about 3 lines, the same pattern as latest_snapshot at lib.rs:185, and the harness already works around the hang at tests/harness/src/ws.rs:24. If the WS feed stays as the interim sync (the open needs_decision), fix (5) now and put (1)-(4) in the design brief.
- Fix: Do not build a typed SDK stream, WSS, in-band invalidation notices, or a casing fix now. Add these five items to the new-sync design brief so they are designed once. The server-side cursor (canonical_history_page plus claim) probably survives the redesign; see E4.
- Decision: Does the existing WS feed stay as the interim sync for Bart's client until the new design lands, or is it frozen immediately?

### E4. The canonical application-history reader lives under storage/egress but is shared with native restart

`low · confirmed · +0 lines · S`  
Where: `sequencer/src/storage/egress/canonical.rs:40-143`, `sequencer/src/storage/egress.rs:18-73`, `sequencer/src/ingress/inclusion_lane/catch_up.rs:65`, `sequencer/src/commands/setup/fill.rs:409`

- Claim: canonical_history_page, history_bounds, ApplicationInputRow, L2TxContext and decode_application_input (storage/egress/canonical.rs:40-143, storage/egress.rs:18-73) are the single application-history reader. Native restart catch-up (ingress/inclusion_lane/catch_up.rs:54,65) and setup fill (commands/setup/fill.rs:409) both use it. Minor correction: history_bounds lives in canonical.rs:41-43/63-71, not next to next_executed_input_count_in. The move target is storage/history.rs, a single file today, or a new history/ directory. The stale module doc l2_tx_feed/mod.rs:4 should be fixed in the same pure move (see ES1).
- Fix: Pure move: put canonical.rs and the row/context/decode types under storage/history/ next to history_bounds and next_executed_input_count_in. storage/egress then holds only historical.rs, which E1/E2 freeze. No behavior change.

### E5. L2TxFeed duplicates http::storage_task's terminal-fault policy with two near-identical error enums

`low · confirmed · -90 lines · S`  
Where: `sequencer/src/egress/l2_tx_feed/mod.rs:80-109`, `sequencer/src/egress/l2_tx_feed/mod.rs:121-141`, `sequencer/src/egress/l2_tx_feed/error.rs:11-87`, `sequencer/src/http.rs:224-266`, `sequencer/src/egress/l2_tx_feed/mod.rs:47-53`

- Claim: prepare() (l2_tx_feed/mod.rs:80-109) re-implements storage_task (http.rs:224-249). SubscribeError and SubscriptionError (error.rs:11-83) are four-variant twins of what persistent_storage_error (http.rs:251-266) already classifies. open_error_is_persistent is an alias (error.rs:85-87). task is always Some (mod.rs:145), so the early return in finish (169-171) is dead. runtime_scope() has one caller (subscribe.rs:89), and its select arm duplicates Subscription::recv's shutdown arm (mod.rs:157-161). send_ws_event warns on an infallible serialization of Strings and integers (subscribe.rs:127-133 vs the expect at :63). Also dead: the `Join => source.is_panic()` arms (error.rs:46, 80). Panics abort before any Join is built (mod.rs:95-106, 136-139), and SubscriptionError::Join is built only in finish() (mod.rs:175), after the classification at :132. Moving to storage_task keeps the lock-retention guarantee: it holds a RuntimeScope clone (http.rs:234), which retains the process lock (shutdown.rs:28-33). Persistent-fault detection still works because HistoryReadError::Storage exposes its rusqlite source via #[from] (canonical.rs:36-37).
- Fix: Run prepare through storage_task and, in the handler, downcast HistoryReadError::Policy to the 409 the same way api/history.rs:149-161 does. Reduce SubscriptionError to one small enum checked with persistent_storage_error. Destructure self in finish(). Drop the shutdown arm in run_ws_session, runtime_scope(), and send_ws_message; use expect in send_ws_event. Do this only when the feed is next touched (see E3).
- Decision: Worth doing only if the server-side feed cursor survives the sync redesign; otherwise leave it frozen.

### E6. WS and history error logs drop their SQLite cause and mislabel what failed

`low · confirmed · +0 lines · S`  
Where: `sequencer/src/egress/api/subscribe.rs:72`, `sequencer/src/egress/api/subscribe.rs:121-123`, `sequencer/src/egress/l2_tx_feed/error.rs:13-22`, `sequencer/src/egress/l2_tx_feed/error.rs:56-66`, `sequencer/src/storage/egress/historical.rs:36-37`

- Claim: The feed's error Displays drop their SQLite cause. 'cannot open subscription storage' and 'cannot load feed head offset' (error.rs:13-22) and 'cannot load ordered tx page from offset {offset}' (error.rs:61-66) all keep the cause only in #[source]. That loses it in the warn logs (subscribe.rs:72, 122) and, more importantly, in the terminal abort message. For a persistent fault, mod.rs:92 and :133 call abort_terminal with `{error}`, and abort_terminal logs only Display (runtime/shutdown.rs:16-18). The process therefore aborts on, say, a corrupt database with no SQLite reason in the log. By contrast, storage_task paths keep the cause, because StorageOpenError is transparent (storage/mod.rs:267-270) and HistoryReadError::Storage formats {0} (canonical.rs:36). The From impl (error.rs:32-38) actively degrades the more informative HistoryReadError. HistoricalReadError::Storage's 'reading historical L1 inputs' label (historical.rs:36) does include the cause and is only mislabelled for /history. The fix (include {source}, or adopt E5) changes log text only.
- Fix: Include {source} in these Display strings, or log through an error-chain formatter. Rename the end-of-session log to 'tx feed subscription ended with error'. Rename LoadHeadOffset to ReadPage. Use neutral 'reading history' wording. This changes log text only.

### E7. SDK dead surface and a misnamed transport error

`low · partially · -35 lines · S · public-rust-api`  
Where: `sdk/rust-client/src/errors.rs:12-48`, `sdk/rust-client/src/errors.rs:62-87`, `sdk/rust-client/src/lib.rs:68-75`, `sdk/rust-client/src/lib.rs:81-86`, `sdk/rust-client/src/lib.rs:150-162`

- Claim: Dead SDK surface is confirmed. SubmitTxError::{TimeoutWrite,TimeoutFlush,IoWrite,IoFlush,Parse} (errors.rs:16-31) are never constructed: map_reqwest_error (lib.rs:238-250) and the body-read maps are the only constructors. SubscribeError::InvalidEndpoint (errors.rs:83-84) is never constructed. host_port (lib.rs:68-75) has no callers. QueryError::Transport wraps SubmitTxError (errors.rs:65-68; lib.rs:150). There are two more mapping defects. Body-read failures, including reqwest per-request timeouts, are mapped to IoRead rather than TimeoutRead (lib.rs:111-114, 159-162), so benchmark breakdown keys (tests/benchmarks/src/rejection.rs:23) mislabel timeouts as io_read. A client build failure is reported as ClientBuildError::InvalidEndpoint (lib.rs:55-56). The with_request_timeout sub-proposal is wrong. The rebuild differs from the constructor's successful build only in the timeout values, which reqwest's build() does not fail on, so the expect at lib.rs:84 encodes an impossible state. Under AGENTS.md that fails loud by design, and returning a Result would add public-API churn for an unreachable error.
- Fix: Rename SubmitTxError to TransportError, drop the dead variants, InvalidEndpoint and host_port, and make with_request_timeout return a Result. Defer unifying the seven error enums into one {route, transport | status+typed ApiError body | decode | history} type until the N24 split creates separate public and internal clients.

### E8. The 50,001-row 'no total cap' feed test duplicates pure validate() coverage

`low · confirmed · -50 lines · S`  
Where: `sequencer/src/egress/l2_tx_feed/tests.rs:86-134`, `sequencer-core/src/history.rs:414-434`, `sequencer/src/storage/egress/canonical/tests.rs:309`

- Claim: subscription_accepts_more_than_fifty_thousand_inputs_without_a_total_cap (l2_tx_feed/tests.rs:87-136) builds 50,001 rows and reads 3 events. The feed adds no cap of its own: prepare and run only call canonical_history_page (mod.rs:87, 198). The absence of a cap is already pinned by core history.rs:415-434 (0, 100_001, 0), and deep paging by canonical/tests.rs:309-335, which also uses 50,001 rows. Delete the feed test or shrink it to a few rows.
- Fix: Delete the test, or shrink it to a few rows with page_size 1 if an end-to-end feed check is wanted.

### E9. The egress contract is written out three times (README, openapi.yaml, protocol docs)

`medium · confirmed · -250 lines · M · docs-only`  
Where: `README.md:271-312`, `README.md:314-440`, `openapi.yaml:236-474`, `docs/protocol/application-history.md:138-177`, `docs/protocol/projection-replay.md:1-125`

- Claim: The egress contract is duplicated across README.md:271-440, openapi.yaml:236-474 and the protocol docs. Verified examples: the order of refusal checks at README.md:290-293 and openapi.yaml:250-253; 'disconnect without a Close frame' at README.md:304-306, openapi.yaml:260-262 and application-history.md:173-177; the SDK prose at README.md:436-440 and application-history.md:160-164. application-history.md:11-12 and projection-replay.md:6-9 both name the README as owner, so changing the owner to openapi.yaml must update those links too.
- Fix: Make openapi.yaml the owner of wire, limit and refusal detail. Cut the README API section to a route table (public vs internal) plus one paragraph per side with links, and keep only semantics in the protocol docs. Do this together with the E1-E3 freeze: replace the frozen subsections with a link and a 'frozen pending sync redesign' note rather than editing their prose.
- Decision: Confirm that openapi.yaml, not the README, is the owner of the wire contract (the PR #50 review raised ownership).

### E10. Stale and restating module comments in egress

`nit · unverified-nit · -8 lines · S · docs-only`  
Where: `sequencer/src/egress/mod.rs:4-6`, `sequencer/src/egress/api/subscribe.rs:4-6`, `sequencer/src/egress/api/health.rs:26-28`, `sequencer/src/http.rs:142-143`

- Claim: egress/mod.rs says 'WS subscribe (today), future read-only endpoints', but /history, /historical-l1-inputs and the snapshot routes already exist. subscribe.rs:5-6 restates RAII ('permit is held for the lifetime of the session and released on disconnect via Drop'). health.rs:26-28 restates the struct's two fields.
- Fix: Reduce egress/mod.rs to one line naming the internal surface. Delete the RAII sentence and the field restatement. Point the http.rs split comment to the register entry instead of 'the api split'.

### ES1. WS feed module and test still claim to serve native catch-up

`nit · skeptic-added · +0 lines · S · docs-only`  
Where: `sequencer/src/egress/l2_tx_feed/mod.rs:4`, `sequencer/src/egress/l2_tx_feed/tests.rs:139`, `sequencer/src/egress/l2_tx_feed/tests.rs:185`, `sequencer/src/ingress/inclusion_lane/catch_up.rs:65`

- Claim: The l2_tx_feed module doc says the feed is 'used by WS subscriptions and catch-up replay', and the lock test is named cancelled_catchup_prepare_... and asserts 'detached catch-up preparation must retain process ownership'. Catch-up does not use L2TxFeed: it calls canonical_history_page directly. Anyone freezing or deleting the feed under E3 would wrongly conclude that native restart depends on it.
- Fix: In the E4 pure move, reword mod.rs:4 to 'DB-backed ordered-L2-tx feed for WS subscriptions'. Rename the test to cancelled_subscription_prepare_retains_process_lock... and fix its assert message.

Keep (load-bearing):

- Mandatory HistoryClaim (era, generation, next_input) on every WS subscribe (subscribe.rs:30-52; core history.rs:185-209). Do-not-simplify 'Don't retain a numeric resume offset without its history identity' (invariants.md:464-465, I18): recovery reuses suffix offsets.
- Per-page claim revalidation inside one read transaction (l2_tx_feed/mod.rs:196-210, canonical.rs:47-60). This is the actual safety gate; the pre-upgrade prepare() only exists to return a typed 409 before the 101.
- X-History-Error header duplicating the 409 body (subscribe.rs:61-69). WebSocket clients may stop reading after the failed upgrade's headers; the SDK reads the header first (sdk lib.rs:289-302), and a test pins this (lib.rs:409-423).
- Gap, length and i64::MAX+1 guards in canonical_page_in (canonical.rs:82-134): 'impossible states fail loud', on a reader shared with native restart catch-up (catch_up.rs:65).
- Generation cuts and the complete-lineage assert (storage/history.rs:98-148): the in-era standard-recovery mechanism; it survives the direction, and a missing cut must never authorize a partial minimum (I18).
- base_executed_input_count / next_executed_input_count_in (storage/history.rs:85-96): the sequencer's own absolute offsets after a rebuild. This is N12's base C, needed by native replay regardless of the client design.
- Process-lock retention by detached feed blocking work (l2_tx_feed/mod.rs:82, 121-141; test tests.rs:139-205): data-directory ownership must outlive cancelled requests.
- Subscriber cap (64) checked before claim validation (state.rs:42-49, http.rs:145). Each subscriber pins a blocking thread polling every 20 ms; the cap bounds blocking-pool use and SQLite polling (about 3,200 small read transactions per second at the cap, acceptable for internal consumers).
- HistoricalResponseBody tying the admission permit to the body's lifetime (egress/api/history.rs:116-176): bounds in-flight page memory to 8 pages. Keep until E1 removes the route; do not polish.
- Health probes on the internal listener (health.rs:11-12): operators and the kubelet probe from inside the cluster.

## F. L1 reader, batch submitter, provider

Verdict: Area F covers the reader, the submitter and the shared provider code. The reader ingests safe InputBox events after a chain-id gate. It checks each batch two ways: index contiguity, and a getNumberOfInputs count pinned at the safe block. The submitter is a stateless tick. It re-derives the unresolved suffix from the safe-accepted frontier plus a Latest scan, raises the wallet-nonce watermark durably before broadcasting (I14), and re-estimates fees on every tick. The content-identity check and the divergence freeze (I9/I15) run in the same transaction as the reader's append. Production code is about 3,300 lines, roughly 23% of them comments. The core mechanisms are proportionate and well chosen. The "large files" are mostly misplaced tests: reader.rs is about 813 production lines plus about 810 lines of tests. l1_submission.rs is only 271 production lines; about 1,000 of its 1,240 test lines exercise check_danger and populate_safe_accepted_batches, which live in other modules. poster.rs is about 546 production lines. The biggest real problems are classification and observability under failure. Self-trust violations and impossible states are routed into the transient "provider/transport" bucket. Provider errors are stringified with no operation, nonce or stage. They are logged at error level on every poll tick and, through reqwest's Display, carry the full RPC URL. The rest is leftover cleanup: a redundant poster-level shutdown select left over from 288ed14, a test-only partition model that duplicates the production bisect, per-nonce pending-age bookkeeping where tracking only the head nonce would do, dead public storage helpers, and rationale prose repeated three or four times. Keep the double input-completeness witness, the write-before-broadcast watermark, the same-transaction divergence freeze, the single encode path shared by seal and submit, and the no-internal-retry stateless tick.


### F1. Self-trust violations and impossible states are classified as transient provider errors

`low · disputed · -15 lines · S · BC`  
Where: `sequencer/src/l1/submitter/poster.rs:532-539`, `sequencer/src/l1/submitter/worker.rs:244-266`, `sequencer/src/l1/reader.rs:458-465`, `sequencer/src/l1/reader.rs:740-748`

- Claim: Three things are true. (a) The impossible usize/overflow paths at reader.rs:458-465 and 740-748 are wrapped as InputReaderError::Provider. The codebase's own pattern for this is expect("...: contract-impossible") (storage/ingress.rs:254, storage/l1_inputs.rs:432). (b) worker.rs:245-265 re-validates the poster's answer and reports the mismatch as a retryable Provider error, which the self-trust section rules out ('no re-validation of a neighbor module's answer', threat-model README.md:34). (c) An undecodable own-key batch maps to Provider (poster.rs:538-539). The claim overstates the impact, though. Nothing here is 'retried silently': worker.rs:193 logs each occurrence at error level. The undecodable-landing stall also clears on its own once the landing passes safe, because the scan starts at safe_block+1 (worker.rs:215-218) and populate already treats undecodable copies as scheduler no-ops (safe_accepted_batches.rs:222-225). None of this has a reachable correctness effect. It is classification and labelling of impossible events.
- Fix: First patch (refactor only): replace the impossible usize/overflow paths with expect() and a reason, and turn the worker's poster-count self-checks into debug_assert! or delete them. Second patch (behavior change): give the own-key undecodable batch its own variant carrying tx hash and InputAdded index. Decide whether it is terminal (the self-trust rule) or skipped in the fold to mirror the scheduler's no-op treatment. Do not label it a provider error either way.
- Decision: For an undecodable batch from our own key: terminal exit (the self-trust rule) or skip it in the nonce fold (mirroring the scheduler's no-op)?
- Independent trace: refuted. The code reads as the reviewer describes.

### F2. Worker error logs lack operation, nonce and stage, fire at error level every tick, and mislabel SQLite faults

`low · confirmed · +10 lines · S`  
Where: `sequencer/src/l1/reader.rs:318-319`, `sequencer/src/l1/reader.rs:495`, `sequencer/src/l1/reader.rs:798`, `sequencer/src/l1/reader.rs:806`, `sequencer/src/l1/submitter/worker.rs:188-195`

- Claim: As stated, with one sharper detail. BatchPosterError::Watermark is #[error(transparent)] (poster.rs:70-71), and WalletNonceWatermarkError::Storage/OpenStorage use #[source] with fixed Display text (watermark.rs:25-28). worker.rs:193 logs `error = %source`, so a SQLite BUSY on the watermark writer is logged as 'L1 provider error — will retry' with only 'watermark sink storage write failed' and no rusqlite cause. The divergence marker persists safe_input_index (safe_accepted_batches.rs:362-371), so the tx hash can be recovered with a join. Adding it to the log is a convenience, not a gap.
- Fix: Prefix each Provider string with its operation and key fields (e.g. 'estimateGas nonce=N', 'eth_getBlock(safe)'). Use a small map_err helper per module, not a new error framework. In worker.rs, log Watermark failures under their own message. Add transaction_hash, plus our hash and the landed hash, to the divergence error log. Whether to downgrade the per-tick retry logs to warn after the first occurrence is the maintainer's call. Fix URL leakage through the register entry (see notes_triage), not here.
- Decision: Keep error level on every retry tick, or log error once and then warn at a lower rate?

### F3. l1_submission.rs is 1,510 lines because about 1,000 test lines belong to other modules

`low · confirmed · +0 lines · S`  
Where: `sequencer/src/storage/l1_submission.rs:509-940`, `sequencer/src/storage/l1_submission.rs:942-1510`, `sequencer/src/storage/recovery.rs:233`, `sequencer/src/storage/recovery.rs:643`, `sequencer/src/storage/safe_accepted_batches.rs:131`

- Claim: Production code in l1_submission.rs ends at line 271, and tests start at 272. Roughly 850 test lines, not 1,000, belong elsewhere: check_danger tests at 525-940, and populate/divergence/seal-hash tests at 941-1315 and 1390-1510. pending_batches_skips_invalidated_and_respects_min_nonce (1316) and nonce_is_reused_after_torn_cascade (1347) test this module's own pending_batches and should stay. check_danger's tests live in a separate file (`mod tests;` at storage/recovery.rs:643). The local helpers default_test_protocol and unix_now_ms (509-523) must move with the tests.
- Fix: Move the check_danger tests into storage/recovery's tests module, and the populate, divergence and seal-hash tests into safe_accepted_batches.rs tests. Pure move: no code changes and a separate patch.

### F4. Poster-level shutdown select is a redundant leftover of 288ed14

`low · confirmed · -40 lines · S`  
Where: `sequencer/src/l1/submitter/poster.rs:68-69`, `sequencer/src/l1/submitter/poster.rs:81`, `sequencer/src/l1/submitter/poster.rs:160-161`, `sequencer/src/l1/submitter/poster.rs:372-381`, `sequencer/src/l1/submitter/poster.rs:442-447`

- Claim: EthereumBatchPoster races every send against the same RuntimeScope that the submitter's run_forever already biased-selects on. The poster's BatchPosterError::Shutdown arm can never reach a caller in production.
- Fix: Remove the poster's shutdown field, externalize_provider_call, the Shutdown variant and its classification arm, and the dedicated test. Cancellation stays with the worker's run_forever select. Do it as a refactor-only patch.
- Decision: Confirm no planned non-worker caller (e.g. a future one-shot submit command) relies on the poster cancelling itself.

### F5. The watchdog parity fixture drives a test-only copy of the bisect, not the production code

`low · partially · -60 lines · S`  
Where: `sequencer/src/l1/partition.rs:68-98`, `sequencer/src/l1/partition.rs:108-178`, `sequencer/src/l1/partition.rs:180-184`, `sequencer/src/l1/partition.rs:230-239`, `sequencer/src/l1/partition.rs:212-218`

- Claim: Core confirmed. partition_vector_simulation_matches_watchdog (partition.rs:321-353) drives the cfg(test) recursive model (116-178), so the production worklist (68-98) is only claimed to match it in prose (64-67). A generic bisect driven by the fixture is the right fix, though the parity test then becomes async. Two parts of the proposal are weaker than stated. sort_logs_by_l1_order is the seam the log-sort parity test calls (partition.rs:359). Deleting it only moves a tautology, because neither version tests production's key closure (219-225). The better fix is to make that key closure the shared function. The reader's missing-block_number branch (reader.rs:684-688) is unreachable through the ordered fetch (partition.rs:213-218), but it is pinned by a unit test (reader.rs:1347). ingest_input_added is documented as a pure function driven by synthetic pairs (reader.rs:673-675). Converting it to expect means deleting that test. That is acceptable under self-trust, but it is not free.
- Fix: Make the worklist generic over its query closure (async fn bisect(from, to, splittable, query)). Use it in production, and drive it in the parity test with a closure that replays the fixture. Delete simulate_partitioned_get_logs and the two one-line wrappers. Change the reader's duplicate check to expect("ordered fetch guarantees block_number").

### F6. Per-nonce pending-age map where tracking only the head nonce would do

`nit · partially · -35 lines · S · BC · docs-only`  
Where: `sequencer/src/l1/submitter/poster.rs:117-144`, `sequencer/src/l1/submitter/poster.rs:150-155`, `sequencer/src/l1/submitter/poster.rs:181-189`, `sequencer/src/l1/submitter/poster.rs:201-234`, `sequencer/src/l1/submitter/poster.rs:465-473`

- Claim: Tracking only the head nonce is not equivalent to the documented contract. docs/l1-fee-policy.md:46-50 promises a per-nonce clock from first attempt that 'Accepted re-broadcasts do not reset'. Under head-only tracking the clock resets whenever Latest advances, so a slowly draining suffix (each nonce mining just under 5 min) never warns, even though its tail batch has been unresolved for k×5 min. The per-nonce map is the minimal structure that keeps first-attempt times for upcoming nonces. The real defect is noise: during a stall every tracked nonce warns independently (poster.rs:203-218), giving k warnings per interval. Warning only for the lowest tracked entry, which holds the oldest clock, fixes that with almost no line change and no doc change.
- Fix: Replace the map with Option<(head_nonce, since, last_warned)>: reset when Latest's nonce changes, warn after PENDING_WAIT_WARN_AFTER, and log the refusal info once per head. Update the fee-policy paragraph to say 'Latest nonce has not advanced for N minutes'. Patch only once the warn semantics are agreed.
- Decision: Is the 'unresolved since first attempt, per nonce' clock worth keeping over 'head stuck for N minutes'?

### F7. Dead public storage helpers, a test-only error variant and a stale module doc

`low · confirmed · -25 lines · S`  
Where: `sequencer/src/storage/l1_submission.rs:4-13`, `sequencer/src/storage/l1_submission.rs:54-56`, `sequencer/src/storage/l1_submission.rs:108-121`, `sequencer/src/storage/l1_submission.rs:124-129`, `sequencer/src/commands/setup/fill.rs:322`

- Claim: Confirmed, with two corrections. frames_for_batch_in is production code (l1_submission.rs:243, via load_batch_frames_in); only the Storage::frames_for_batch wrapper (127-129) is test-only. The getter Storage::batch_tree_anchor (l1_submission.rs:59) is also test-only (fill.rs:207/288 in its test module, l1_inputs.rs:671-685); the e2e/harness batch_tree_anchor calls are the harness's own methods. WalletNonceWatermarkError::Other is constructed only at poster.rs:766 and watermark.rs:97, both tests. The test sink can return Storage(SQLITE_BUSY) instead.
- Fix: Mark the three methods cfg(test), or delete them and have the tests call the _in helpers. Fix the module doc. Have the test sink inject a busy rusqlite error instead of keeping the Other variant.

### F8. Cockroach-recovery seams in the reader and its storage: freeze, don't polish

`low · partially · +0 lines · S`  
Where: `sequencer/src/l1/reader.rs:138-141`, `sequencer/src/l1/reader.rs:225-231`, `sequencer/src/storage/l1_inputs.rs:69-152`, `sequencer/src/storage/l1_inputs.rs:176-183`, `sequencer/src/storage/l1_inputs.rs:266-268`

- Claim: The freeze recommendation stands, but the inventory needs fixing. open_tip_nonce (l1_submission.rs:110-121) is test-only, per F7, so it is not a live recovery seam: delete it rather than freeze it. The missing-baseline branch in populate (safe_accepted_batches.rs:166-177) also serves plain setup, per its own comment ('Plain setup detects prior activity before installing genesis'), so it is not cockroach-only. The real seams are FrontierMode::DeferUntilAnchorSet and set_frontier_mode (reader.rs:138-141, 225-231; sole caller setup/mod.rs:262), first_batch_submitter_input_after_block (l1_inputs.rs:85; setup/mod.rs:705) and safe_inputs_in_block_range (l1_inputs.rs:120; setup/mod.rs:488/494). Together they come to about 100 production lines.
- Fix: Freeze: no doc or abstraction polish on these items. Carry two constraints into the tool's design. First, it must reuse the reader's verified ingest rather than a fresh log scan (the warning at l1_inputs.rs:74-79 remains true). Second, the batch-tree anchor (I16) survives in any design, because a tool-produced sequencer state still roots at N'.

### F9. Rationale repeated three or four times, design-history prose, and table cells over 150 words

`low · partially · -120 lines · M · docs-only`  
Where: `sequencer/src/l1/mod.rs:16-29`, `sequencer/src/l1/provider.rs:127-134`, `sequencer/src/l1/submitter/poster.rs:46-50`, `sequencer/src/l1/submitter/poster.rs:394-402`, `sequencer/src/l1/reader.rs:38-44`

- Claim: The mid-life chain-failover rationale really is stated three times: provider.rs:127-134, poster.rs:46-50 and poster.rs:394-402, the last one tagged '(review)'. The design-history prose at l1/mod.rs:23-29, safe_accepted_batches.rs:122-130 and l1_inputs.rs:41-42 is as described. reader.rs:38-44 is not a repeat, though: it covers in-model operator URL repointing across restarts. The 'Uniswap row' is threat-model README.md:24 (about 208 words); line 25 is the 17-word batch-submitter-key row. The RPC row (:22) is about 175 words. The Uniswap row belongs to the fee-oracle area. The failover rationale itself names a scenario the threat model excludes (see FS1), so trimming it is a content decision, not just a wording one.
- Fix: State each rationale once: chain-id window at create_verified_signer_provider; poster and reader link to it in one line. Delete the design-history sentences. In the threat model, cut each actor row to trust level plus a one-line capability, and move the completeness-witness and oracle-outage arguments into short subsections. Leave the F8 items untouched.

### F10. SubmitterExit is a one-variant enum that every caller maps away

`nit · unverified-nit · -10 lines · S`  
Where: `sequencer/src/l1/submitter/worker.rs:69-78`, `sequencer/src/commands/run/workers.rs:144`, `sequencer/src/commands/run/workers.rs:353`, `sequencer/src/commands/run/workers.rs:619-621`

- Claim: SubmitterExit::Shutdown is the only variant, and the supervisor immediately pattern-maps it to ().
- Fix: Return Result<(), BatchSubmitterError> like the reader does.

### FS1. Chain-id gating is asymmetric: the poster defends against mid-life failover, but the reader checks only once

`low · skeptic-added · -6 lines · S · BC`  
Where: `sequencer/src/l1/reader.rs:146`, `sequencer/src/l1/reader.rs:344-348`, `sequencer/src/l1/reader.rs:488-502`, `sequencer/src/l1/reader.rs:394-401`, `sequencer/src/l1/submitter/poster.rs:46-50`

- Claim: The poster re-checks eth_chainId before every productive send, specifically to survive a load-balanced RPC that fails over to another chain mid-life. The reader is the component whose ingest writes durable state, yet it verifies the chain id only once per instance (the chain_id_verified flag). Its comment says the check runs 'before reading anything from it'. Either mid-life failover is in scope, in which case the reader is the weaker gate, or it is out of scope ('must be one consistent node'), in which case the poster's gate and its three rationale comments defend an excluded scenario without naming that assumption.
- Fix: Decide the model first. If failover is in scope, drop the chain_id_verified flag and check every tick. That is stateless, about -6 lines and one extra eth_chainId per poll. If it is out of scope, reword poster.rs:46-50/394-402 and provider.rs:131-134 to name the assumption and a revisit trigger, and fix the overstated reader comment.
- Decision: Is a mid-life RPC chain failover inside the supported trust model?

### FS2. The watermark raise does a blocking SQLite open and durable write on the async submitter and flusher tasks

`low · skeptic-added · +6 lines · S · BC`  
Where: `sequencer/src/l1/submitter/poster.rs:426`, `sequencer/src/recovery/flusher.rs:263`, `sequencer/src/l1/watermark.rs:63-73`, `sequencer/src/storage/open.rs:171`, `sequencer/src/l1/submitter/worker.rs:274`

- Claim: StorageWatermarkSink::raise_to opens a writer connection with a 5 s busy_timeout and commits a durable MAX upsert synchronously. It is called inline from async submit_batches and from the flusher. The submitter routes every other SQLite access through spawn_blocking_with_lock. Under writer contention with the lane, this ties up a tokio worker thread for up to 5 s, and the biased shutdown select cannot preempt it.
- Fix: Run the sink call through spawn_blocking_with_lock, in the poster or by making the sink own the lock and the blocking hop, and await it before the first send. Write-before-broadcast ordering is preserved, and the MAX upsert is idempotent if a cancelled raise finishes in the background. Do this in the same patch that settles the register's SQLite-contention entry, so both paths get one policy.

Keep (load-bearing):

- Double input-completeness witness in the reader (reader.rs:375-405, 438-466): index contiguity proves the right prefix, and getNumberOfInputs pinned at the scanned safe block proves the prefix is complete. It is the only defense against truncated get_logs from a lagging endpoint. The count-first fast path is the same trust base and saves a long paginated crawl.
- Log/payload cross-check in ingest_input_added (reader.rs:678-724): block number and index are both carried by the log and by the EvmAdvance payload, and must agree. It mirrors the watchdog's decode_and_validate_log.
- Three chain-id gates (reader.rs:488-504 once per reader, poster.rs:403-413 per non-empty tick, provider.rs:135-154 for keyed one-shot writers): they protect different windows (ingest, long-lived signing, one-shot signing). Keep the code and trim the repeated prose (F9).
- Write-before-broadcast watermark (watermark.rs:43-74, poster.rs:422-426, l1_submission.rs:85-97): I14. A single MAX upsert committed with synchronous=FULL is the whole zombie-prevention anchor for the flush.
- Content-identity check and divergence marker written in the same transaction as the reader's append (safe_accepted_batches.rs:146-154, 225-245, 296-335): I9/I15. The early return on an existing marker is the frontier freeze. The per-invocation scan cursor (113-120) is deliberate so a recovery batch reusing a nonce is not skipped.
- load_batch_frames_in as the single encode path shared by seal and submitter (l1_submission.rs:231-270): the seal-time hash must come from the same bytes the submitter broadcasts, or I15 raises false mismatches.
- Stateless submitter tick that re-derives everything from the safe frontier plus a Latest scan, with no internal retry (worker.rs:207-269): on the Do-not-simplify list (docs/invariants.md:468-470). The orchestrator respawn is the retry mechanism.
- is_pending_tx_conflict classification (poster.rs:329-350): lets the rest of the suffix go out past a held nonce. An unrecognised wording falls back to the old behavior, so it is safe.
- pad_gas_estimate (eip1559.rs:20-25): an out-of-gas revert would burn a wallet-nonce slot with no InputAdded.
- L1-order sort plus refusal of logs missing a block number, at one point (partition.rs:192-228): both the reader's contiguity witness and the submitter's nonce fold need canonical order.
- Operator-configurable long-range error codes for the bisect (partition.rs:11-26): range-error messages are not stable across providers. The overload with rate-limit codes is documented and accepted.
- App-deployment-block scan floor gated on the v3 InputBox version() witness (reader.rs:171-190, 601-649): turns a potentially enormous genesis crawl into a bounded one, and is sound only under the verified v3 semantics.
- HTTPS enforcement with an explicit insecure opt-in, and SubmitterKey redaction (provider.rs:30-50, l1/mod.rs:46-71): cheap guards against operator foot-guns and key leaks.

## G. Automatic recovery and cockroach recovery (setup --recovery)

Verdict: This area does two separate jobs. The first is automatic startup recovery: the detector exits, the orchestrator respawns, startup inspects, syncs, picks one repair under a guarded transaction, inspects again, and runtime admission checks a final time. The second is cockroach rebuild via `setup --recovery`: load a checkpoint, flush, re-sync, fold, publish a baseline. Production code is about 2,600 lines. The large file sizes are mostly tests (flusher.rs is ~405 prod / ~530 test, recovery/mod.rs ~505/~560, fill.rs ~75/~540). The automatic-recovery core is proportionate to its real concurrency and durability complexity: one dispatch table, Immediate transactions that re-inspect before mutating, a flush observation kept only on the stack, and a typed retry/refuse split. I found no high-severity bug. The real defects are a two-call TOCTOU in the flusher's completion observation (a fail-loud backstop exists) and a leftover plain-setup `--checkpoint-block` that turns an early clean refusal into a later, absorbing CanonicalDivergence. The biggest structural problem is that cockroach recovery is woven into `setup` (~270 prod lines in setup/mod.rs, ~450 counting config, errors and tests). That is exactly what the planned shared tool replaces, so it should be frozen. The pieces that survive (Checkpoint load, the seed/replay split, `fold_replay`, `fill_recovery_state`) already form an almost pure seam. N1 is cheap (~45 lines) with a genesis selector. docs/recovery/README.md is accurate and tight, but code comments repeat themselves (the DangerStatus arms are described four times) and carry stale history.


### G1. Freeze `setup --recovery` in-band orchestration (~450 lines incl. config/errors/tests)

`medium · partially · -450 lines · L · config`  
Where: `sequencer/src/commands/setup/mod.rs:405-673`, `sequencer/src/commands/setup/checkpoint_tests.rs:1-226`, `sequencer/src/commands/config.rs:378-453`, `sequencer/src/commands/error.rs:387-430`, `sequencer/src/recovery/mod.rs:79-86`

- Claim: Freezing the in-band `setup --recovery` orchestration is right. The code that only cockroach recovery uses is `Checkpoint`/`load`/`source_fold_inputs`/`recover`/`rebuild_from_checkpoint`/`flush_wallet_nonce`/`to_fold_input` (sequencer/src/commands/setup/mod.rs:405-673), the recovery config and validation (config.rs:378-453), `SetupRecoveryError` (error.rs:387-430), the frontier toggle (setup/mod.rs:255-263), `CheckpointAheadOfStop` (recovery/mod.rs:79-86, raised only at setup/mod.rs:571), and `assert_resync_caught_up` (recovery/mod.rs:136, called only at setup/mod.rs:555). The docs scope is mis-stated, though. cockroach.md:140-160 'Application-specific reader state' (restore the client checkpoint, replay L1 through C, drain, then agree on K) is not removed by the direction. It is the manual form of the client half that the shared tool will automate, and G2 itself relies on it (cockroach.md:142-144), so keep it as that half's spec. docs/protocol/projection-replay.md is mostly the general egress bootstrap: /history, replay from genesis, subscribe at K (projection-replay.md:11-31), plus standard-recovery generation compatibility (:44-56). The new sync supersedes it, but it belongs to the egress area and is not cross-era cockroach machinery.
- Fix: Freeze: fix only safety bugs. Do not polish its error classification (including the re-sync site of the open register entry), load ordering, receipt format, or SetupRecoveryError wording. About 150 of these lines move into the tool library (see G2); the rest is deleted at extraction, not now.
- Decision: Confirm the freeze scope covers the projection-replay cross-era docs as well as the setup code.

### G2. Extraction seam for the shared recovery tool: keyless, generic over Application, C supplied

`medium · partially · -250 lines · L · BC · config`  
Where: `sequencer/src/commands/setup/mod.rs:560-614`, `sequencer/src/commands/setup/mod.rs:405-499`, `sequencer-core/src/scheduler/fold.rs:4-20`, `sequencer-core/src/scheduler/fold.rs:62-75`, `sequencer/src/commands/setup/fill.rs:36-50`

- Claim: The pure seam is real. `rebuild_from_checkpoint` only sources `safe_inputs`, folds, and fills (setup/mod.rs:560-614). `fold_replay` is side-effect free and already expects a future standalone tool (sequencer-core/src/scheduler/fold.rs:12-15). The checkpoint loads only after the full sync (setup/mod.rs:534 vs 264), and B<=C is checked only after the signed flush (569 vs 537). The C-sourcing claim is wrong. `flush-mempool` does not return C: the command returns `()` and only logs `safe_block` (commands/flush.rs:23, 78-83). It also cannot run in either cockroach scenario. On a wiped DB it needs a completed setup (lifecycle.rs:250-252). On the surviving diverged DB its preflight refuses on canonical divergence (commands/flush.rs:30 -> lifecycle.rs:141). A keyless tool therefore needs a setup-less flush mode: submitter address plus an optional watermark, emitting C in machine-readable form. Either that, or the tool embeds the flush. The rest of the design is a reasonable plan, but it is a future-tool design, not a refactor of today's code.
- Fix: (1) Library: `recover::<A>(Checkpoint<A>, inputs in (A,C] ascending, C, submitter) -> {app S', N', K, clock}`. Move into it Checkpoint+load (405-465), the seed/replay split (467-499), the A<B<=C checks (449-457, 569-577) and to_fold_input (664-673). (2) The tool never signs. C comes from the existing `flush-mempool`, and the tool refuses unless its L1 view reaches C. Run all deterministic checks (load, B <= synced head) before any L1 work. (3) Sequencer output: `setup --baseline <bundle>` publishes (S', N', K, C) through `fill_recovery_state`/`complete_baseline_setup`. Genesis becomes the trivial baseline. Setup loses --recovery, key args, the DeferUntilAnchorSet toggle, `flush_wallet_nonce`, `recover`, and SetupRecoveryError. (4) Client contract: the client runs the same library with its own richer `A`, from its own checkpoint (boundaries may differ, cockroach.md:142-144), to the same C. Agreement means equal (K, last_executed_safe_block) per I19 and equal N'. The receipt (era, C, N', K) is what the future sync binds to. Not worth investing in now: streaming fold inputs (setup/mod.rs:487-497 collects into a Vec, though fold accepts iterators), checkpoint.toml/finalized_snapshot receipt polish, setup-recovery error classification.
- Decision: Should C come from `flush-mempool` (keyless tool, recommended) or should the tool embed the flush? Should the library live in sequencer-core or a new crate? Should the tool write the sequencer DB directly or emit a bundle for `setup --baseline`?

### G3. N1: genesis recovery needs a hand-made bundle; add a genesis checkpoint selector

`medium · confirmed · +45 lines · S · BC · config`  
Where: `sequencer/src/commands/config.rs:425-428`, `sequencer/src/commands/setup/mod.rs:428-464`, `sequencer/src/commands/setup/mod.rs:318-330`, `sequencer/src/egress/api/snapshot.rs:205-218`, `sequencer/src/commands/setup/mod.rs:839-909`

- Claim: Today `--recovery` requires `--checkpoint-dump-dir` holding info.toml, checkpoint.toml, and app state. checkpoint.toml is produced only by egress `/finalized_snapshot`. Plain setup cannot be used to mint a genesis artifact on a chain with prior batches, because its gate refuses before the genesis app is built. The loader already accepts genesis (`is_genesis`, setup/mod.rs:449-452), and the binary already has the genesis constructor (setup/mod.rs:47-50, used at 329), but recovery never receives it.
- Fix: Add `--checkpoint genesis` (or: --recovery with B=0 and no dump dir) that builds `Checkpoint{app: genesis_app()?, A:0, N:0, B:0}`, reusing the count-zero check from fill.rs:18-28. Then remove the `is_genesis` exception from `Checkpoint::load`, so supplied bundles must strictly satisfy A<B. Cost: ~45 lines plus one test. Runtime cost: a full replay from app deployment, with all inputs held in memory (setup/mod.rs:487-497). That is acceptable until history is large; revisit if setup's RSS binds. Trust caveat: the result is canonical only if this binary's scheduler and app match the canonical machine over the whole history (the same trust as any (B,C] replay, over a longer range).
- Decision: Add it now to frozen setup (~45 lines, immediately useful), or only as the genesis case of the future tool.

### G4. Plain `setup --checkpoint-block B>0` is a leftover knob that defers a clean refusal into absorbing divergence

`low · disputed · -10 lines · S · BC · config`  
Where: `sequencer/src/commands/config.rs:368-377`, `sequencer/src/commands/setup/mod.rs:233-246`, `sequencer/src/commands/setup/mod.rs:318-330`, `sequencer/src/commands/setup/mod.rs:704-705`, `sequencer/src/commands/setup/fill.rs:29-30`

- Claim: Plain `setup --checkpoint-block B>0` is a dead, misleadingly documented knob (config.rs:368-375 'PR3 ...'), but the failure is not deferred to a later sync. Plain setup syncs with `FrontierMode::Populate` (only recovery defers, setup/mod.rs:261-263), with base 0 before completion (safe_accepted_batches.rs:166-177) and anchor 0. A previously accepted batch therefore fails content identity as Foreign (safe_accepted_batches.rs:301-309) and records the divergence marker during setup's own initial sync. The gate then passes because the batch is <= B, and `register_genesis_baseline` -> `complete_baseline_setup` refuses on that marker (lifecycle.rs:104-111). So setup still refuses before completion, with the same remedy (wipe + cockroach). It just shows a divergence refusal instead of `BatchPastCheckpoint` (exit 40 'setup needs recovery'). If the prior batches were all scheduler-rejected, genesis is correct. The impact is diagnostic only. A simpler fix than adding validation: delete `--checkpoint-block` entirely. In recovery it only cross-checks the receipt the operator copies it from (setup/mod.rs:436-441; cockroach.md:178 '--checkpoint-block <block-from-receipt>'), so B can come from checkpoint.toml. Keep the B-vs-deployment check (setup/mod.rs:239) for recovery.
- Fix: Reject `checkpoint_block != 0` without --recovery in `SetupConfig::validate` (config.rs:434-443). Call the gate with 0, and drop the plain-path B-vs-deployment-block check. Under G2, the flag moves to the tool entirely.
- Independent trace: refuted. The claim says plain setup with B>0 'defers a clean refusal into absorbing divergence' that 'a later sync records'.

### G6. `TipAlreadyOpen` is documented as impossible yet handled as Retry

`low · confirmed · -8 lines · S · BC`  
Where: `sequencer/src/storage/recovery.rs:115-119`, `sequencer/src/storage/recovery.rs:267-269`, `sequencer/src/recovery/mod.rs:89-90`, `sequencer/src/recovery/mod.rs:497-499`

- Claim: The variant doc says it is 'unreachable under the process lock, and retryable if it ever fires'. Retrying an impossible state goes against the 'impossible states fail loud' principle: the next boot re-inspects, finds Ready, and admits, hiding the only cause that can produce this state (a lock violation).
- Fix: Classify it as refuse (or `assert!` inside the transaction, which rolls back on unwind), and delete `RecoveryRetryReason::TipAlreadyOpen`. This changes the exit code from 20 to 30 for a state that should never occur.

### G7. DangerStatus arms restated four times; flush docs narrate each other's callers

`low · confirmed · -60 lines · S · docs-only`  
Where: `sequencer/src/storage/recovery.rs:37-54`, `sequencer/src/storage/recovery.rs:57-83`, `sequencer/src/storage/recovery.rs:173-232`, `sequencer/src/recovery/flusher.rs:124-134`, `sequencer/src/commands/setup/mod.rs:620-628`

- Claim: The DangerStatus arms are described in the enum-level list (storage/recovery.rs:37-54), the per-variant docs (57-83), the numbered list (173-205), and the trust-hierarchy list (206-228). The enum-level list is also incomplete: it omits `CanonicalDivergence`. Caller narration appears at flusher.rs:128-134 and setup/mod.rs:625-628, and meta commentary at detector.rs:12-16. Keep one ordering paragraph that cites I4, because the arm order is on the Do-not-simplify list (docs/invariants.md:459-461).
- Fix: Keep one-line variant docs plus one ordering paragraph citing I4. Delete the caller narration and meta commentary.

### G8. Stale code docs: 'shared with the batch submitter', 'PR3', `flush_and_wait` as the entry point

`low · confirmed · -10 lines · S · docs-only`  
Where: `sequencer/src/storage/recovery.rs:10-12`, `sequencer/src/storage/recovery.rs:436`, `sequencer/src/commands/config.rs:372-375`, `sequencer/src/commands/error.rs:394-396`, `sequencer/src/recovery/flusher.rs:4-10`

- Claim: Confirmed as stated, plus two more stale lines in the same vein. commands/flush.rs:9 says 'the same flush `setup --recovery` will run internally' in the future tense, although it already does (setup/mod.rs:653). The SetupRecoveryError doc (error.rs:388-390) says the post-flush re-sync uses the transient RecoveryError paths, but the re-sync's sync failure maps to `WorkerExit::InputReader` (setup/mod.rs:546-553). Only the caught-up check uses RecoveryError (555).
- Fix: Correct the wording; mark the test-only visibility as such.

### G9. Log levels contradict the documented semantics (flush timeout, detector exit, per-slot send errors)

`nit · partially · -5 lines · S`  
Where: `sequencer/src/recovery/flusher.rs:380-389`, `sequencer/src/recovery/flusher.rs:236-247`, `sequencer/src/recovery/detector.rs:152-157`, `sequencer/src/recovery/detector.rs:35-37`, `sequencer/src/recovery/flusher.rs:354`

- Claim: Only the flusher timeout contradiction is clear. flusher.rs:381-389 logs error! with 'should not happen during normal L1 operation', while flusher.rs:236-239 calls the same timeout 'the flush's ordinary rhythm, not a fault'. That deserves debug/warn. The detector point is debatable. detector.rs:35-37 says 'not an error' about the exit classification (a distinct CommandError, exit 10), not about severity. A danger detection stops soft confirmations, and the statuses include `CanonicalDivergence`, so a blanket warn would under-report divergence. If anything, choose the level per status. The per-slot error! (flusher.rs:354) is redundant only for the first three nonces; the aggregate carries just three samples (95-115), so the per-slot lines are the only record of the rest.
- Fix: Log the watch timeout at debug (keep the loop's single warn). Lower the detector to warn. Drop the per-slot error!, since the aggregate already carries three samples.

### G10. Stringified transport errors can embed credential-bearing RPC URLs

`low · confirmed · +10 lines · S`  
Where: `sequencer/src/recovery/flusher.rs:283`, `sequencer/src/recovery/flusher.rs:324`, `sequencer/src/recovery/flusher.rs:353-354`, `sequencer/src/recovery/flusher.rs:402`, `sequencer/src/l1/provider.rs:146`

- Claim: Confirmed. Fold it into the open register entry 'Startup logs the full RPC URL' as a scope extension; it is not a separate work item.
- Fix: Extend that register entry's scope to transport error strings and redact once at the provider boundary (strip the URL), rather than fixing each site. Verify with a synthetic credential URL.

### G11. Flush waits on its own tx hash instead of slot consumption

`low · confirmed · -25 lines · S · BC`  
Where: `sequencer/src/recovery/flusher.rs:367-395`, `sequencer/src/recovery/flusher.rs:117-122`, `sequencer/src/recovery/flusher.rs:265-270`, `sequencer/src/recovery/flusher.rs:62-66`

- Claim: Accurate, but the race is narrower than implied. No-ops are sent only from `nonce_at(Latest)` upward (flusher.rs:254, 265), so an original tx already mined gets no competing no-op. The 10-block wait (derive_timeouts, 62-66) happens only when the original or a zombie is mined between that read and the broadcast, or outbids the 3x-headroom bumped no-op. Expect about -15 to -20 lines rather than -25.
- Fix: Replace watch_txs with a bounded wait (same timeout, polling every safe_poll_interval) for `nonce_at(Latest) >= flush_end`. Delete map_watch_error, the hash vector, and the PendingTransactionConfig use. This adds no retry loop (the Do-not-simplify rule is respected): errors still propagate and the outer loop is unchanged. Gain: up to 10 blocks only in the race where the original lands, so it is justified as a simplification, not as a performance fix.

### G12. Three guarded recovery mutations repeat the same transaction preamble

`nit · partially · -20 lines · S`  
Where: `sequencer/src/storage/recovery.rs:248-278`, `sequencer/src/storage/recovery.rs:282-306`, `sequencer/src/storage/recovery.rs:312-338`

- Claim: The preamble is duplicated three times (storage/recovery.rs:253-260, 288-295, 318-325), but the proposed closure helper hides each method's commit point in safety-critical guarded mutations and nets only about -8 to -12 lines. A plainer helper, `begin_guarded(&mut self, protocol, now_ms) -> Result<(Transaction<'_>, RecoveryInspection), RecoveryMutationError>`, would open the Immediate tx, inspect, refuse divergence, and require the checkpoint. Each method would keep its own guard and an explicit `tx.commit()`.
- Fix: Add a private `guarded(&mut self, protocol, now_ms, body: FnOnce(&Transaction, RecoveryInspection) -> Result<T, RecoveryMutationError>)` that commits on Ok, keeping each action-specific guard in its body. Pure refactor.

### GS1. Cockroach flush throws away a surviving durable watermark

`low · skeptic-added · +10 lines · S · BC · config`  
Where: `sequencer/src/commands/setup/mod.rs:652`, `sequencer/src/commands/flush.rs:30`, `sequencer/src/storage/lifecycle.rs:141`, `sequencer/src/recovery/flusher.rs:160-172`, `docs/recovery/cockroach.md:246-250`

- Claim: The flusher's only defense against zombie batch txs (dropped from the local pool but alive elsewhere) is the durable watermark anchor `safe >= watermark + 1`. `setup --recovery` reads the watermark from the freshly wiped DB, which is None on a first attempt. `flush-mempool` cannot be pointed at the surviving old data dir either: its preflight refuses on canonical divergence, which is the usual reason for a cockroach recovery. So even when the old DB is intact (every divergence-triggered rebuild), recovery falls back to the provider's pool view, and cockroach.md presents the watermark as unavailable ('The lost database cannot supply').
- Fix: Do not change frozen setup. In the shared tool (G2), let the flush step take an optional read-only watermark from the old DB (`SELECT watermark FROM wallet_nonce_watermark`) and use the max of that and any fresh watermark. This is a stateless read with no new machinery. Invariant protected: every wallet slot the previous deployment may have broadcast is consumed at safe depth before C. Quantified: it matters only when a zombie exists at rebuild time, which is rare, but the existing fallback is a second full cockroach. Update cockroach.md:246 to say 'when the database is lost'.
- Decision: Should the shared tool's flush accept the old data dir (or its watermark value) as an optional input?

Refuted:

- G5. Flusher reads the safe nonce and C in two separate Safe-tag calls. No reachable scenario under the supported trust model.

Keep (load-bearing):

- RuntimeAdmission witness plus the final admit_runtime re-inspection (recovery/mod.rs:110-116, 374-386): preparation can outlast the L1 view's freshness; only the final check authorizes launch.
- Provider-failure fallback on the initial sync only, and none after the flush (recovery/mod.rs:267-276 vs 305): the cascade needs a view that has actually caught up to the flush observation.
- Flush observation kept only on the stack, so a retry flushes again (recovery/mod.rs:302-304): a persisted witness could go stale across restarts.
- Guarded Immediate transactions that re-inspect before mutating (storage/recovery.rs:248-338): clock-dependent facts can change between the decision and the mutation; StaleDecision is reachable.
- check_danger arm order and the two find_* helpers (storage/recovery.rs:388-427): on the Do-not-simplify list (I4).
- Unconditional post-flush cascade of the whole non-gold closed suffix (storage/recovery.rs:449-457): a convergence policy; keeping the suffix can re-enter the same danger cycle (pinned by recovery/mod.rs:843-894).
- Generation advance inside cascade_and_reopen (storage/recovery.rs:492-494): I18 offset reuse needs history identity. Freeze it until the new client sync is designed, but do not remove it.
- Watermark anchor `safe >= watermark+1` in the flush completion predicate (flusher.rs:203-208): I14, it covers zombie txs the local pool forgot.
- No internal retry loop in the flusher (flusher.rs:298-303): Do-not-simplify; respawn is the retry.
- RecoveryL1 trait (recovery/mod.rs:212-217): the one seam that lets procedure tests run real SQLite inspections and repairs.
- Detector as its own worker holding a ProcessLock clone (detector.rs:73-80, 165-178): blocking checks keep data-directory exclusivity after shutdown races.
- Checkpoint receipt agreement checks (setup/mod.rs:436-446) and fold.rs's always-on ordering assertions: cheap fail-loud guards on trusted inputs. They move to the tool rather than being deleted.
- Exhaustive retry/refuse classifiers with polarity tests (recovery/mod.rs:388-504, 536-641): they pin exit-code semantics for the supervisor.

## H. Commands, config, errors, runtime, storage foundation, logging (cross-cutting)

Verdict: This area holds the operator surface: clap config for setup, run and flush-mempool, the exit-code taxonomy (10/20/30/40/1 plus SIGABRT 134), the run bracket and worker supervisor, the process lock and shutdown scope, the storage open/lifecycle foundation, and the baseline schema. Its size is mostly proportionate. About 4,200 Rust prod lines plus 660 SQL lines sit in scope. error.rs is about 727 prod and 600 test lines. config.rs is about 607 prod and 490 test lines. In both, the bulk is doc comments (31% and 26% of prod lines) and the verdict table test, not machinery. The exit-code projection is the essential part: one exhaustive CommandFailureVerdict feeds both the black box and the exit code, and it should stay. The biggest problems are diagnostic, not structural. (1) Root causes get lost: the error types mix two Display conventions, and every sink prints only Display. (2) io::Error values at data-dir and lock boundaries reach the operator with no path. (3) Expected exits (10/20) and per-poll retryable provider errors log at ERROR, which breaks ERROR-based alerting. The one real config hazard is N16: MAX_WAIT_BLOCKS is a binary constant that the deployment identity does not pin. Freeze the setup --recovery CLI and error surface, since the shared recovery tool supersedes it. The schema's triggers and CHECKs mostly earn their keep because they enforce registered invariants at the engine layer.


### H1. Root causes vanish from exit, abort and black-box lines (two Display conventions, Display-only sinks)

`medium · confirmed · +12 lines · S · BC`  
Where: `sequencer/src/ingress/inclusion_lane/error.rs:17-44`, `sequencer/src/l1/watermark.rs:25-28`, `sequencer/src/egress/l2_tx_feed/error.rs:15-20`, `sequencer/src/egress/l2_tx_feed/error.rs:58`, `sequencer/src/harness.rs:129`

- Claim: The claim holds. InclusionLaneError (error.rs:17-44), CatchUpError (error.rs:94-103), WalletNonceWatermarkError (watermark.rs:25-28) and SubscribeError/SubscriptionError (l2_tx_feed/error.rs:13-21, 57-67) use #[source] with a Display that omits the cause. Every sink prints only Display. Two details are missing. (a) WorkerExit (commands/error.rs:513-526) and WorkerStop::Source (error.rs:572-573) embed `{0}` without #[source], so source() stops at WorkerStop, and a chain-walker would also need those types changed. That makes embed-in-Display the consistent choice. (b) SubscribeError::OpenStorage also reaches abort_terminal through l2_tx_feed/mod.rs:92,105, which prints `preparing tx-feed subscription: cannot open subscription storage`. SubscribeError::Join embeds `{source}` while also marking it #[source] (l2_tx_feed/error.rs:23-27), so it would print twice under a walker.
- Fix: Use one convention. The cheap option is to append `: {source}` to the ~10 format strings that omit it, which matches the convention CommandError and BootstrapError already use. Add one test that a nested lane error's Display contains its io-level root. Example: before `ERROR terminal runtime failure; aborting process error=inclusion lane: snapshot at batch close failed`; after `... error=inclusion lane: snapshot at batch close failed: <dump error>: No space left on device (os error 28)`. A chain-walking reporter is the alternative, but only if the embedded `{0}` is removed everywhere, otherwise causes print twice.
- Decision: Choose embed-in-Display (small) or source()+chain-walker (repo-wide). Recommend embed-in-Display.

### H2. Data-dir, lock and sweep io::Errors reach the operator with no path

`low · partially · +25 lines · S · BC`  
Where: `sequencer/src/commands/error.rs:51-52`, `sequencer/src/commands/error.rs:633`, `sequencer/src/runtime/process_lock.rs:66-72`, `sequencer/src/commands/run/mod.rs:51`, `sequencer/src/commands/flush.rs:24`

- Claim: Confirmed in code: CommandError::Io (error.rs:51-52) and ProcessLockError::Io (process_lock.rs:40-41) are transparent. create_dir_all (run/mod.rs:51, flush.rs:24), the lock open (process_lock.rs:66-72), try_exists (commands/mod.rs:42) and read_dir (startup_hygiene.rs:85-86) therefore surface bare OS messages and exit 1. The diagnostic loss is smaller than claimed. These are the first filesystem actions of a single-data-dir command, and the operator supplied the path through --data-dir or the env, so the path can almost always be inferred. Carrying the path is a cheap papercut fix, not a medium defect.
- Fix: Carry the path at these ~6 sites. Make ProcessLockError::Io a struct variant `{ path, source }`. Add a `DataDir { path, source }` CommandError variant for create_dir_all, try_exists and read_dir, classified Unclassified exactly like Io today. Example: before `ERROR sequencer exiting error=Permission denied (os error 13) exit_code=1`; after `ERROR sequencer exiting error=data-directory lock /var/lib/seq/sequencer.lock: Permission denied (os error 13) exit_code=1`. Add a row to the error.rs unclassified() table so the verdict stays pinned.

### H3. Expected exits (10/20/40) and per-poll retryable provider errors log at ERROR

`low · partially · +12 lines · S · BC`  
Where: `sequencer/src/harness.rs:125-131`, `sequencer/src/l1/submitter/worker.rs:193`, `sequencer/src/l1/reader.rs:312-319`, `sequencer/src/runtime/shutdown.rs:17`

- Claim: error! is used for every CommandError exit (harness.rs:129) and for each retried provider failure in the submitter (worker.rs:193, once per idle tick) and in the reader (reader.rs:312-319). The documented alerting contract, however, is the exit code (error.rs:61-73 projection header; docs/watchdog/operator-deployment.md), not log level, and the repo states no log-level policy. Exit 10 (danger detected) is an operationally significant event, not routine noise. The strongest part of the finding is the per-tick ERROR during an L1 outage, one line every 2-5 s. Downgrading retry logs to warn and adding a verdict field on the exit line is a reasonable low-priority cleanup.
- Fix: Pick the level from the verdict: Terminal and Unclassified log at error; ExpectedRecovery, Retryable and SetupRecoveryRequired log at warn. Add a `verdict` field that names the supervisor action. Downgrade the per-tick retry logs to warn. Add `exit = "SIGABRT(134): terminal, do not restart"` to the abort line. Example: before `ERROR sequencer exiting error=worker exited: danger detector: danger detected (TipInDanger(41)) — stopping for startup recovery exit_code=10`; after `WARN sequencer exiting for supervised restart verdict=ExpectedRecovery exit_code=10 error=...`.

### H4. MAX_WAIT_BLOCKS is a binary constant the deployment identity does not pin (N16)

`low · disputed · +60 lines · M · BC · db-schema`  
Where: `sequencer-core/src/lib.rs:21`, `sequencer/src/commands/config.rs:74-81`, `sequencer/src/storage/mod.rs:239-246`, `sequencer-core/src/scheduler/mod.rs:33-38`, `examples/canonical-app/src/bin/canonical-app-devnet.rs:11`

- Claim: MAX_WAIT_BLOCKS (sequencer-core/src/lib.rs:21) is compiled into both the guest (scheduler/mod.rs:33-38) and the sequencer (config.rs:74-81), and DeploymentIdentity does not pin it (storage/mod.rs:239-246). The hazard is real but generic. AGENTS.md:74 makes 'the same source compiled into the on-chain machine' the model, and any build skew in sequencer-core scheduler rules (staleness formula, fee math, DOMAIN_NAME) diverges the same way. Pinning one constant covers one vector of many. The constant has not changed since it was introduced (git log -S: only 2bd2e62). The proposed `setup --max-wait-blocks` flag is worse than the status quo. It turns a single compile-time source of truth into an operator input that the sequencer cannot check against the guest's per-binary constant, which adds a new divergence path at setup. If build-skew detection is wanted, pin a compiled protocol fingerprint (MAX_WAIT_BLOCKS, DOMAIN_NAME, a protocol version) at setup and refuse a mismatch at run, with no operator knob.
- Fix: Do not add a free runtime knob. Give it the same shape as the sequencer address: a per-deployment value. (1) Add a `SchedulerConfig::new(addr, max_wait_blocks)` constant per guest binary. (2) Add `setup --max-wait-blocks` (env CARTESI_SEQUENCER_MAX_WAIT_BLOCKS, default 1200), pin it in deployment_identity and include it in mismatch_fields. (3) In run, build ProtocolTiming after load_setup_identity from the pinned value, which moves run/mod.rs:57 below :64. Tests keep using the literal.
- Decision: Schema and identity change (ask-first area). Confirm guest images take it per-binary like the sequencer address.
- Independent trace: refuted. Every claim in the evidence holds: the constant is unpinned and the guest and the sequencer share it.

### H5. Freeze the setup --recovery CLI, error and lifecycle surface; the shared recovery tool replaces it

`medium · partially · +0 lines · S`  
Where: `sequencer/src/commands/config.rs:286-331`, `sequencer/src/commands/config.rs:378-453`, `sequencer/src/commands/error.rs:388-434`, `sequencer/src/storage/lifecycle.rs:36`, `sequencer/src/storage/lifecycle.rs:112-119`

- Claim: Freezing is right for the CLI and validation surface: OptionalKeyArgs (config.rs:286-331), the recovery fields and validate()/resolve_recovery_key (config.rs:378-453), and SetupRecoveryError. Those leave setup when the shared tool lands. The claim overreaches on the storage side. The tool must still produce the sequencer's startup DB, and that is exactly what LifecycleCommand::Rebuild plus complete_baseline_setup(recovery=true) do (initialize_for_command, open_recovery_tip_in_tx; lifecycle.rs:100-130). That layer probably moves behind the tool rather than being deleted, so 'resolves Overlapping admission policies by removal' is not assured. Plain setup's checkpoint_block detection scan and the exit-40 refusal also stay meaningful, because they point the operator at the tool.
- Fix: Do not polish any of it: no KeyArgs/OptionalKeyArgs unification and no SetupRecoveryError cleanup. When the tool lands, delete it from setup. That also resolves the 'Overlapping admission policies' register entry (Rebuild arms) by removal. Size in this area is about 200 prod lines plus about 150 test lines.

### H6. L1/data-dir args are copied across three configs with identical docs, despite the module's own shared-group rule

`low · partially · -35 lines · S`  
Where: `sequencer/src/commands/config.rs:15-16`, `sequencer/src/commands/config.rs:339-356`, `sequencer/src/commands/config.rs:462-481`, `sequencer/src/commands/config.rs:547-567`

- Claim: Confirmed: eth_rpc_url and allow_insecure_rpc, with an identical 5-line doc, are repeated in SetupConfig (config.rs:339-351), RunConfig (:464-476) and FlushConfig (:549-555), against the module's own shared-group rule (config.rs:15-16). The line delta is overstated. The duplication is about 13 lines × 3 = 39. A flattened L1RpcArgs plus three flatten sites costs about 23, for a net of roughly -15 to -20, not -35. FlushConfig's separate seconds_per_block is intentional, because flush does not take the rest of TimingArgs.
- Fix: Add `#[derive(Args)] struct L1RpcArgs { eth_rpc_url, allow_insecure_rpc }` and flatten it into all three configs. Leave data_dir in place, or move it into the same group. Flag names and env vars stay the same.

### H7. Test-only code in production modules: set_alpha and complete_setup

`low · partially · -40 lines · S`  
Where: `sequencer/src/storage/admin.rs:15-34`, `sequencer/src/storage/migrations/0001_schema.sql:567`, `sequencer/src/storage/lifecycle.rs:145-171`

- Claim: complete_setup is #[cfg(test)] (lifecycle.rs:150) and has only two test callers (:376, :438), so it can go. Only the divergence assertion (:375-378) should be repointed to complete_baseline_setup. The test at :436-440 checks complete_setup's own precondition, which is meaningless for complete_baseline_setup because that function creates the baseline itself, so delete that test rather than repointing it. set_alpha has no in-repo non-test caller, but it is a `pub fn` on Storage in `pub mod storage` (sequencer/src/lib.rs:30), so moving it into test_helpers is a public-rust-api change for downstream app crates, not a free move. Fixing the schema comment (0001_schema.sql:567) is uncontroversial. Net delta is about -30.
- Fix: Move set_alpha into storage/test_helpers and drop admin.rs. Reword the schema comment to 'constant; tests override'. Delete complete_setup and point its two tests at complete_baseline_setup.

### H8. run/flush create a mistyped data dir, then refuse with a message that names no path

`low · confirmed · +5 lines · S · BC`  
Where: `sequencer/src/commands/run/mod.rs:51-55`, `sequencer/src/commands/flush.rs:24-27`, `sequencer/src/commands/mod.rs:42-44`, `sequencer/src/commands/error.rs:343-344`, `sequencer/src/storage/open.rs:51-58`

- Claim: run (run/mod.rs:51-55) and flush (flush.rs:24-27) create the data dir and sequencer.lock before preflight_lifecycle_command finds no DB (commands/mod.rs:42-43). They then exit 30 with SetupNotComplete, whose Display names no path (error.rs:343-344). The open.rs rule cited (open.rs:51-58) concerns DB files, so this is a spirit-level inconsistency, not a violation. The proposed existence check before the lock is a read ahead of the lock, which the run/mod.rs:52-54 comment says to avoid. It is benign, though: absence refuses, and presence proceeds to take the lock.
- Fix: Only setup creates the data dir. In run and flush, a missing dir maps to SetupNotComplete before the lock is taken. Give SetupNotComplete a `data_dir` field shown in Display. Exit code stays 30.

### H9. The terminal_faults black box records only causes that re-log anyway

`low · partially · -150 lines · M · BC · db-schema`  
Where: `sequencer/src/storage/migrations/0001_schema.sql:447-482`, `sequencer/src/storage/lifecycle.rs:70-76`, `sequencer/src/storage/lifecycle.rs:173-231`, `sequencer/src/commands/mod.rs:50-113`, `sequencer/src/commands/setup/mod.rs:380-402`

- Claim: The black box captures only terminal verdicts returned through a command bracket (commands/mod.rs:57-79). In run, terminal worker faults abort instead (workers.rs:375, 458), so most recorded causes are startup-phase and are re-derived on the next boot. This is ADR mechanism 2, registered as policy in docs/invariants.md:32-43, and the ADR itself states the limitation (2026-08-authority-boundary-adr.md:88-91). The finding adds no new evidence beyond the ADR's accepted trade. The 'set looks empty' claim is also overstated. Config-derived terminal verdicts (ChainIdMismatch or FeeOracleMisconfig in setup, SignerMisconfig at workers.rs:653) are not re-derived once the operator changes the config. The box also has a live consumer, the boot echo (run/mod.rs:62). Leave it to the maintainer, and note HS1, a concrete defect in that consumer.
- Fix: Either keep it as an accepted cost (it is ADR-recorded) or delete it and let the exit code plus logs carry the cause, which H1-H3 make reliable. Quantify first: count how many terminal verdicts are not re-derived on the next boot. Today that set looks empty.
- Decision: ADR mechanism 2. Maintainer decides keep vs delete; do not change it silently.

### H10. Startup event omits the timing knobs; orphan-sweep deletions are not audited

`low · confirmed · +8 lines · S · BC`  
Where: `sequencer/src/commands/run/mod.rs:138-151`, `sequencer/src/commands/run/startup_hygiene.rs:21-25`, `sequencer/src/commands/run/startup_hygiene.rs:98-99`

- Claim: The startup event (run/mod.rs:138-151) omits max_batch_open_seconds, l1_read_stale_after_blocks, seconds_per_block and confirmation depth. The orphan sweep logs successful deletions only as a debug count (startup_hygiene.rs:20-24, 98-99). Per-path info logging matters most for the sweep, because of its known path-spelling hazard (startup_hygiene.rs:64-65: 'setup and run may spell the same directory differently'). The eth_rpc_url removal belongs to the open register entry, not to this finding.
- Fix: Add those fields to the startup event and drop eth_rpc_url (see the register entry). Log each orphan deletion at info with its path: one line per destructive action.

### H11. batch_submitter_idle_poll_interval_ms accepts 0 and turns the idle submitter into an RPC hot loop

`low · confirmed · +1 lines · S · BC · config`  
Where: `sequencer/src/commands/config.rs:485-491`, `sequencer/src/l1/submitter/worker.rs:198-202`, `sequencer/src/l1/submitter/worker.rs:219-222`

- Claim: batch_submitter_idle_poll_interval_ms (config.rs:485-491) has no range parser, unlike poll_interval_ms (config.rs:112) and max_batch_open_seconds (:509). With 0, the idle and transient arm (submitter/worker.rs:198-201) sleeps for zero and re-runs load_frontier plus observed_submitted_batch_nonces RPCs back-to-back.
- Fix: Add `value_parser = clap::value_parser!(u64).range(1..)`.

### H12. Danger-exit terminality is defined twice (abort decision vs exit verdict)

`nit · unverified-nit · -4 lines · S`  
Where: `sequencer/src/commands/error.rs:201-225`, `sequencer/src/commands/error.rs:556-561`

- Claim: WorkerExit::is_terminal hard-codes `CanonicalDivergence | Safe` for DangerDetected, and danger_failure_verdict separately maps the same statuses to Terminal. The abort decision (workers.rs:373) and the exit code could drift apart.
- Fix: Write `WorkerExit::DangerDetected { status } => danger_failure_verdict(status).is_terminal()`.

### H13. Comments restate code, repeat each other, or carry change history

`low · partially · -120 lines · S · docs-only`  
Where: `sequencer/src/commands/error.rs:74-90`, `sequencer/src/commands/error.rs:489-495`, `sequencer/src/commands/config.rs:368-377`, `sequencer/src/storage/migrations/0001_schema.sql:21-26`, `sequencer/src/storage/migrations/0001_schema.sql:117-120`

- Claim: The comment-density figures are correct: 228 of 727 prod lines in error.rs, 159 of 607 in config.rs. The EXIT_SETUP_NEEDS_RECOVERY doc (error.rs:81-89) and the 6-line clippy rationale (error.rs:489-494) are trimmable. Two examples are mischaracterized. The config.rs:369-376 'PR3' sentence is history-flavored wording, but not stale, since it points at `setup --recovery` as the non-genesis path. The schema's 'a CHECK on it once wedged batch close' (0001_schema.sql:22-24) is the rationale for a deliberate absence, a do-not-add guard. Keep the reason and drop only the anecdote. Do the --recovery-adjacent text last or skip it, given the H5 freeze.
- Fix: Trim to the invariant and the why: one line for the Box rationale, one line per exit constant, delete the PR3 sentence, and leave history to git. Do one docs-only pass after H1-H3 so the trimmed text describes the final behavior.

### H14. Schema seed uses INSERT OR IGNORE against the fail-loud check policy

`nit · unverified-nit · +0 lines · S · db-schema`  
Where: `sequencer/src/storage/migrations/0001_schema.sql:601`, `docs/invariants.md:29-31`

- Claim: The check policy forbids `INSERT OR IGNORE`. The batch_policy seed uses it inside a migration that rusqlite_migration runs exactly once, so OR IGNORE can only mask a duplicate-seed bug.
- Fix: Use a plain INSERT.

### HS1. Boot-time black-box echo warns forever about a fault the operator already fixed

`low · skeptic-added · +6 lines · S · BC`  
Where: `sequencer/src/commands/run/mod.rs:62`, `sequencer/src/commands/mod.rs:84-113`, `sequencer/src/storage/lifecycle.rs:196-229`, `sequencer/src/storage/migrations/0001_schema.sql:470-482`, `sequencer/src/commands/setup/mod.rs:359-376`

- Claim: Every `run` boot logs a WARN for the newest terminal_faults row. The table is append-only and nothing marks a fault as resolved. A terminal setup failure on a fresh data dir (for example, a typo'd --chain-id) is therefore echoed as a WARN on every healthy `run` boot for the life of the database, even after a corrected setup retry completes.
- Fix: Have the black box's one consumer say only what is still news. Skip, or log at debug, any fault whose recorded_at_ms predates setup_complete.completed_at_ms, because a later completed setup proves a setup-era fault was resolved. Include the fault's age in the remaining WARN. If H9 deletes the black box, this goes with it.
- Decision: Tied to H9: fix only if the black box is kept.

Keep (load-bearing):

- CommandFailureVerdict as the single classification behind both the exit code and the black box (commands/error.rs:95-195): one exhaustive match, so a new error shape cannot get one class in the logs and another at the supervisor.
- The exit-code table tests (commands/error.rs:806-1292): about 480 lines, but each row pins one supervisor-facing integer per failure shape. This is the wire contract for orchestrators, and the distinct-codes test makes code pins equal verdict pins.
- Per-worker is_terminal_invariant beside each error enum plus the generic WorkerStop<E> (commands/error.rs:529-609): a new variant fails to compile in its own module instead of falling through a distant wildcard.
- is_persistent_storage_error failing open toward operational (storage/convert.rs:121-175): a wrong 'terminal' pages for a self-healing condition, while a wrong 'operational' only restarts.
- SYNCHRONOUS_PRAGMA = FULL and its comment (storage/open.rs:16-32): it protects acknowledged chunk commits, the write-before-broadcast watermark and setup completion against power loss. The comment names exactly what relaxing it would break.
- Storage::open refusing a missing DB file (storage/open.rs:49-72): database absence means uninitialized, which prevents an ownerless schema on a mistyped path. H8 extends the same rule to the directory.
- The kernel process lock as Arc<File> clones plus spawn_blocking_with_lock (runtime/process_lock.rs): exclusivity outlives cancelled async futures that detach blocking DB work, and a persisted row cannot provide that.
- abort_terminal and the panicking ShutdownOnDrop abort (runtime/shutdown.rs:16-19; commands/run/workers.rs:102-111): a terminal runtime fault exits without draining over corrupt state.
- refuse_divergence_on_clean_exit (commands/run/run/mod.rs:96-102 path: sequencer/src/commands/run/mod.rs:96-102): it closes the exit-0 hole where a clean drain outruns divergence re-detection.
- The Workers exhaustive destructures and concurrent drain (commands/run/workers.rs:323-473), already upheld by the jury: they make a forgotten worker a compile error and keep a hung component from hiding a terminal exit.
- Schema invariant triggers and typeof guards (0001_schema.sql:27-33, 110-190, 360-370, 394-445, 526-532, 653-660): they enforce I15 (divergence freeze), I16 (single anchored root), write-once stamps and history immutability at the engine layer. That is cheap, multi-writer-safe defense for a store where divergence equals theft.
- Secret handling at the clap edge (commands/config.rs:249-254, 588-592; hide_env_values on key and RPC args): the key never reaches Display or a log line, even on read failure.
- The TTY-gated ANSI init in examples/wallet-sequencer/src/main.rs:10-18: it keeps escape codes out of journald and file logs (the earlier CI-red cause).

## I. Snapshots, dumps, checkpoint metadata

Verdict: This area turns each batch close into an immutable app-state artifact. The order is: the file is made durable, then one SQLite transaction seals the batch and registers the artifact. Selection is derived from acceptance: restart uses the newest valid batch, the watchdog and recovery use the newest accepted batch. GC deletes rows before files, and a lease lets HTTP serve the watchdog's comparison file and the operator's recovery archive. The core is correct and fails loud where it should. Selection never falls back to an older snapshot, GC keeps every valid snapshot beyond the accepted frontier, the divergence check shares one transaction with selection and lease, and the archive never reports a truncated success. I found no correctness bug. The size is about 1,330 production lines across the five in-scope files, plus a 65-line release supervisor in http.rs. That is roughly 30-40% more than the essential complexity. The excess sits in three places: 1. **Leases:** a lease is a process-local reference count, but it is stored durably in SQLite. That needs egress writer connections, a release supervisor task, a persistent-failure reporter and a startup reset (about 120 lines). 2. **`info.toml`:** the on-disk file repeats what SQLite already knows, and the export receipt repeats it again. It costs one extra file and 2 extra fsyncs per batch close (about 70 lines). 3. **Snapshot-plus-subscribe bootstrap:** `/latest_snapshot` and the history-claim headers serve a client bootstrap that the future direction replaces. They should be frozen, not polished (about 140 production and 250 test lines). Per-batch snapshots (N23) should stay. The cost is one lane stall per batch, and batches are paced by L1 posting. The real risk is a large native state on a filesystem without reflinks, which is a deployment requirement and needs no new policy. Sparse snapshots would trade that cost for a liveness failure in startup, recovery and the watchdog. For N3, the sequencer needs nothing from machine-emulator PR #398: the SQLite row is a stronger publish point than an atomic rename, and the watchdog has already adopted `rename_stored` and `remove_stored`.


### I1. On-disk info.toml duplicates SQLite and the export receipt; drop it and generate it inside archives

`medium · confirmed · -70 lines · S · BC · docs-only`  
Where: `sequencer/src/ingress/inclusion_lane/dump_info.rs:38-60`, `sequencer/src/ingress/inclusion_lane/dump_info.rs:93-101`, `sequencer/src/ingress/inclusion_lane/dump_info.rs:136-149`, `sequencer/src/ingress/inclusion_lane/dump_info.rs:156-205`, `sequencer/src/ingress/inclusion_lane/snapshot.rs:51`

- Claim: info.toml holds only `format_version` and `next_batch_nonce`. Every server path already takes the nonce from SQLite, and the recovery receipt is a strict superset of the file. The file adds an unpublished-directory temp+rename and 2 fsyncs to every batch close, and it versions only itself, not the app bytes.
- Fix: 1. Make the dump dir just mkdir + `app.create_dump(dir/state)` + parent fsync. 2. In `write_archive`, generate `info.toml` from the SQLite nonce it already receives, the same way it generates `checkpoint.toml` (dump_info.rs:108-117). The archive bytes and the recovery loader stay unchanged. 3. Replace the startup parse with a stat of `state`, or drop it. 4. Leave merging info.toml into a single receipt to the shared recovery-tool design, which will own the export format. If the file is kept instead, at least drop the temp+rename and the directory fsync in `write_info`.
- Decision: Should startup keep a cheap stat-based presence check of the rollback artifact, or rely on catch-up's terminal `from_dump` classification? Archive format stays unchanged either way.

### I2. Download leases are stored durably in SQLite but are process-local; replace with an in-memory registry

`medium · confirmed · -120 lines · M · db-schema`  
Where: `sequencer/src/storage/snapshot_dumps.rs:46-97`, `sequencer/src/storage/snapshot_dumps.rs:114-131`, `sequencer/src/storage/snapshot_dumps.rs:182-244`, `sequencer/src/storage/snapshot_dumps.rs:431-451`, `sequencer/src/http.rs:160-206`

- Claim: A lease only means something while this process serves a body, and startup zeroes every count. Even so, each download does 2 SQLite write transactions from egress. Release goes through an injected scheduler, a supervisor task with a JoinSet, and a persistent/transient error classifier that aborts the process. All of that exists to decrement a counter.
- Fix: Share `DumpLeases(Arc<Mutex<HashMap<i64,u32>>>)` between egress and the lane's GC. - **Acquire:** lock, run the read-only selection transaction (divergence check included), increment, unlock. - **Release:** the guard's Drop locks and decrements. It is infallible and does no I/O. - **GC:** lock, select candidates inside its write transaction, skip leased ids, delete, unlock. - **Lock order:** registry before SQLite. Egress never takes the SQLite writer lock, so this cannot deadlock. - **Invariant protected:** a leased artifact's row and files are never deleted. - **Assumption:** only the serving `run` process collects. Revisit if a second process ever serves or collects dumps. This removes the column, the startup reset, the release supervisor and the reporter wiring, and egress becomes SQLite read-only.
- Decision: Approve dropping `dumps.lease_count` and adopting the single-serving-process assumption.

### I3. /latest_snapshot and the history-claim headers serve the snapshot-plus-subscribe bootstrap the new sync replaces: freeze

`medium · confirmed · -140 lines · S · BC · http-api`  
Where: `sequencer/src/egress/api/snapshot.rs:195-202`, `sequencer/src/egress/api/snapshot.rs:392-410`, `sequencer/src/egress/api/snapshot.rs:183-187`, `sequencer/src/egress/api/snapshot.rs:268-276`, `sequencer/src/storage/snapshot_dumps.rs:99-106`

- Claim: `/latest_snapshot` and the `X-History-Era`, `X-Recovery-Generation` and `X-Executed-Input-Count` headers exist so a client can restore the app dump and subscribe with a history claim. The future direction drops that assumption: clients hold their own SQLite state and get startup state from the shared recovery tool.
- Fix: Freeze until the new client sync lands, then delete: - the route - `acquire_latest_snapshot_lease` and `snapshot_next_nonce_in` - `LeasedDump.history_version` and the history headers on all snapshot routes - the SDK method and the openapi entries Keep `Storage::latest_snapshot()`, which catch-up uses (inclusion_lane/catch_up.rs:22-31). Don't polish in the meantime (e.g. the redundant `snapshot_next_nonce_in` re-query).
- Decision: Confirm `/latest_snapshot` and the history headers are frozen until the replacement sync exists.

### I4. ETag/If-None-Match 304 on /finalized_state has no consumer

`low · confirmed · -10 lines · S · BC · http-api`  
Where: `sequencer/src/egress/api/snapshot.rs:150-169`, `sequencer/src/egress/api/snapshot.rs:436-441`, `openapi.yaml:531-560`, `README.md:456`

- Claim: The watchdog idle-polls `/finalized_state/inclusion_block` and compares `/digest`. It never sends `If-None-Match`, so the conditional-GET path and its contract surface are unused.
- Fix: Remove the ETag header, `if_none_match` and the 304 branch, plus the matching openapi/README text and the integration test.
- Decision: Drop the conditional-GET feature from the operator API?

### I5. The per-batch snapshot stalls the lane without any timing recorded; no revisit trigger exists for large native state

`low · partially · +5 lines · S`  
Where: `sequencer/src/ingress/inclusion_lane/snapshot.rs:41-60`, `sequencer/src/ingress/inclusion_lane/mod.rs:152-167`, `sequencer/src/storage/snapshot_dumps.rs:133-160`

- Claim: `create_dump` runs synchronously and untimed on the lane at every close (snapshot.rs:41-60; mod.rs:152-167). That part is true. But closes are not paced by L1 posting. They fire when the fee-policy byte target is reached (`batch_size_target` → `max_batch_user_op_bytes`, storage/mod.rs:354, 371-377; lane mod.rs:435) or after `max_batch_open_seconds` (config.rs:501-511). Under load, several closes can happen per L1 block, so the stall scales with throughput ÷ target size. The register already tracks this: 'representative deployment latency, including checkpoint creation' sits under Track 3 follow-ups (register.md:267-269). Add the elapsed-time field and hang the reflink requirement and revisit trigger on that item instead of adding a duplicate entry.
- Fix: - Log `create_dump` elapsed time at batch close (one debug field). - Add a register 'Known optimizations' entry: the stall is per batch, mitigated by a reflink-capable filesystem (XFS reflink=1, btrfs, APFS) for large-state engines. - Revisit trigger: measured `create_dump` p99 above about 250 ms, or retained-dump disk over budget on deployment storage. This needs no new mechanism.
- Decision: Accept 'reflink-capable filesystem for large-state native engines' as a deployment requirement.

### I6. Snapshot stream failures after the 200 are never logged; error messages omit the conflicting values and the path

`low · partially · +10 lines · S`  
Where: `sequencer/src/egress/api/snapshot.rs:253-263`, `sequencer/src/egress/api/snapshot.rs:316-332`, `sequencer/src/egress/api/snapshot.rs:443-446`, `sequencer/src/ingress/inclusion_lane/dump_info.rs:76-80`, `sequencer/src/ingress/inclusion_lane/dump_info.rs:94-98`

- Claim: Non-terminal mid-stream errors are not logged: the producer only forwards them through `done_tx` (snapshot.rs:253-263), and `ComparisonReader` returns them (snapshot.rs:327-330). tower-http 0.6.11's `TraceLayer::new_for_http` does not log body errors for HTTP classification, because `classify_eos` is None (tower-http trace/future.rs:74-88, body.rs:82-88). But a warn in the producer's non-terminal branch would fire on every client disconnect: once the body is dropped, the duplex writer fails with BrokenPipe (snapshot.rs:240-263). So skip broken-pipe/disconnect errors or log them at debug. The missing path (snapshot.rs:191, 443-446) and the missing values (dump_info.rs:76-80, 94-98) are accurate, although the nonce-mismatch error is InvalidData, so it is logged via `abort_terminal` with the prefix (snapshot.rs:254-258). It only lacks the 2 nonces.
- Fix: - Log a warn with the prefix and error in the producer's non-terminal branch and in `ComparisonReader`'s error map. - Pass the path into `internal_error` for open failures. - Include the expected and found values in the 2 messages.

### I7. Three AsyncRead wrappers where two suffice

`low · partially · -20 lines · S`  
Where: `sequencer/src/egress/api/snapshot.rs:281-287`, `sequencer/src/egress/api/snapshot.rs:297-302`, `sequencer/src/egress/api/snapshot.rs:412-432`

- Claim: `GuardedReader` is pure delegation (snapshot.rs:417-432), and the `Arc` in `stream_body` is unneeded (snapshot.rs:300). For archives, though, the body-side guard is redundant, not just the `GuardedReader`. All source-file reads happen inside `write_archive` on the producer thread, which holds its own guard (snapshot.rs:244-252). After it returns, the remaining bytes sit only in the 64 KiB duplex buffer. Simpler shape: the producer owns the `LeaseGuard` by value, `ArchiveReader` carries no guard, `ComparisonReader` owns its `LeaseGuard`. Delete `GuardedReader` and both `Arc`s, and update lifecycle.md:129-131.
- Fix: Add `_guard` fields to `ComparisonReader` (`LeaseGuard`) and `ArchiveReader` (`Arc<LeaseGuard>`), then delete `GuardedReader`. This composes with I2.

### I8. Snapshot docs and comments restate code, narrate history, or contradict the code

`low · partially · -40 lines · S · docs-only`  
Where: `docs/snapshots/lifecycle.md:38`, `docs/snapshots/lifecycle.md:133-148`, `docs/snapshots/README.md:3-13`, `docs/snapshots/format.md:62-65`, `sequencer/src/ingress/inclusion_lane/dump_info.rs:174-183`

- Claim: These verbosity points hold: - lifecycle.md:38 says "Initially". - lifecycle.md:139-145 restates the inode sweep that the code comments already own (startup_hygiene.rs:66-69, 112-115). - format.md:62-65 narrates a test. - dump_info.rs:174-183 restates `deny_unknown_fields` and the version gate. - dump_info.rs:505-507 narrates "the old line parser". - snapshot.rs:428 explains Unpin soundness the compiler already enforces. The `make_dump_dir` 'contradiction' is overstated. 'Opaque' there means the code never parses the name (snapshot.rs:62-67), and the nonce is a human aid. Reword the comment, which must change anyway if I1 lands because it cites info.toml. Renaming dirs by batch_index is churn without benefit. Folding README.md into lifecycle.md is optional: README is the directory index, and its paragraph at lines 8-13 is not duplicated elsewhere.
- Fix: - Cut the device/inode paragraph to one sentence that links to the code. - Replace "Initially" with "Every". - Fold README.md into lifecycle.md's opening. - Drop the narrated test and history comments. - Name dumps by `batch_index`, the real snapshot identity, plus nanos, or drop the "opaque" claim.

### I9. The wallet nests its single file as state/state; the reference engine should write its file at the prefix

`nit · partially · -8 lines · S · BC · http-api`  
Where: `examples/app-core/src/application/wallet.rs:362-391`, `docs/snapshots/format.md:12-22`

- Claim: The wallet does nest one file as `state/state` (wallet.rs:367-391; format.md:21-22), and the contract permits a file at the prefix (application/mod.rs:253-254; application-contract.md:217-218). The gain is 1 mkdir and 1 directory fsync per close, plus an exemplar. The contract text and the N3 recipe already give libdex that pattern. The change breaks existing wallet bundles, touches the genesis/`--state-file` wiring (tests/harness/src/sequencer.rs:1398, 1448), the C wallet engine that delegates to `WalletApp` (examples/c-wallet-engine/src/lib.rs:407), and the wallet test at wallet.rs:1058. Do it only if the toy wallet is meant to be the reference engine pattern.
- Fix: Write the SSZ file at `prefix` and have `state_file_in_dump` return the prefix itself. Update format.md. Ship it as its own patch, because existing wallet recovery bundles carry the nested layout.
- Decision: Change the toy wallet's dump layout? This breaks existing wallet recovery bundles.

Keep (load-bearing):

- File created and fsynced before the SQLite row, row deleted before the files, and the split between storage/snapshot_dumps.rs (SQLite only) and the filesystem halves. The module boundary is the crash-ordering guarantee (I13, do-not-simplify).
- Exact-snapshot selection with no fallback to an older artifact (snapshot_dumps.rs:162-163, 349-356). A missing artifact is corruption and fails loud (I7).
- GC keeps every valid snapshot beyond the accepted frontier (snapshot_dumps.rs:133-160). An intermediate batch can become the next required checkpoint (do-not-simplify).
- The canonical-divergence check runs in the same transaction as selection and lease (snapshot_dumps.rs:371-375). A matched batch that precedes a divergent one in the same block must not be advertised.
- Only genesis baselines are comparable (snapshot_dumps.rs:385-400). A rebuilt baseline may contain the fold's speculative drain.
- Leases as a concept. Acceptance advancing mid-download lets GC delete the finalized artifact, and NotFound is classified terminal, so the lease prevents a benign race from aborting the process. Only the storage of the lease is over-built (I2).
- `referenced_artifact_io_is_terminal` (dump_info.rs:27-36) and `comparison_io`. They separate a corrupt referenced artifact (terminal) from an operational I/O error (retry).
- `ArchiveReader` turning a producer failure into a stream error (snapshot.rs:334-369). Without it a truncated archive would pass as a successful 200.
- `catch_unwind` around the foreign `state_file_in_dump` callback (snapshot.rs:291-295). Request-task panics would otherwise be swallowed.
- The receipt is generated at export, never stamped into the immutable dump (dump_info.rs:85-117). Artifacts stay immutable.
- The device/inode identity in the orphan sweep (startup_hygiene.rs:72-115). It fixed a path-alias data-loss bug.
- `diagnose_missing_dump`'s watchdog-layout hint (dump_info.rs:207-252). It is cheap and aimed at the most likely operator mistake under recovery pressure.
- Unconditional per-batch snapshots (N23). They are paced by L1 posting, and sparse snapshots would turn a cost into a liveness failure.

## J. Example apps, canonical app, binaries

Verdict: This area holds the placeholder wallet (app-core), the two Rust wallet binaries, the canonical RISC-V guest shell, the C-seam host (c-app-sequencer), the reference C-ABI engine (c-wallet-engine, which wraps WalletApp in unsafe FFI and adds a genesis tool), and the guest test suite (canonical-test). It is mostly proportionate. The 1,080-line wallet.rs is about 400 lines of app and 680 of tests. The app part is fine for a placeholder. The tests are where the excess sits: some duplicate sequencer-core guard tests, some are trivia, and one keeps a "legacy" SSZ type alive just to test an unknown selector. The Rust wallet and the C engine share no business logic: the engine delegates every call to WalletApp, and a native-vs-bridge conformance test checks the two agree. Keep both. The biggest problems are about what an app author would copy: - Anvil test private keys and their parser live in the app crate and are part of its public API (J1). - App state uses HashMap, so canonical ordering depends on remembering to sort in the encoder (J5). - There are two different genesis models. Rust picks a config per binary; C uses a genesis tool plus a `--state-file` flag that actually names a directory (J2, J3). - Tracing setup is copied three times, and the harness doc example calls an `init_tracing()` that does not exist (J4). The genesis plumbing should be frozen, not polished: the planned shared recovery/startup tool replaces it. Things to keep: the engine-delegates-to-WalletApp shape, the fsync'd create_dump, the typed error on incoherent snapshot progress, the golden snapshot vector shared with Lua, and the guest-side fee-arithmetic test.


### J1. Anvil test-key list and its validating parser live in the app crate's public API

`medium · partially · -120 lines · S`  
Where: `examples/app-core/src/application/anvil_accounts.rs:1-87`, `examples/app-core/anvil_accounts.txt:1-100`, `examples/app-core/src/application/mod.rs:9`, `tests/harness/src/rollups.rs:380`, `tests/harness/src/rollups.rs:449`

- Claim: app-core publicly re-exports `default_private_keys` (examples/app-core/src/application/mod.rs:9). Its only callers are tests/harness (rollups.rs:380, :449, :495; sequencer.rs:1368; wallet.rs:46) and tests/benchmarks (workload.rs:69, :187, :311, :369). parse_accounts validates the address column and then discards it (anvil_accounts.rs:49-51), and 32 test lines (88-119) cover a parser of a fixed, checked-in file. The keys can move to tests/harness: benchmarks already depends on rollups-harness (tests/benchmarks/Cargo.toml:13). The mnemonic option is not free, though. The harness enables only alloy's `signer-local` feature (tests/harness/Cargo.toml:14), so it would need `signer-mnemonic` added. The cheapest move is to keep the .txt file under tests/harness and take column 2 with a three-line split. Also update the doc pointer at wallet.rs:83-84.
- Fix: Move the keys to tests/harness as a `const DEFAULT_PRIVATE_KEYS: &[&str]`, or derive them from Anvil's well-known mnemonic with alloy's mnemonic builder, which the harness already depends on. Delete the parser and its tests, and drop the re-export from app-core.

### J2. Two genesis models (per-binary preset vs genesis tool + --state-file): freeze, the shared startup-state tool replaces both

`low · partially · -150 lines · M · BC · config`  
Where: `examples/wallet-sequencer/src/main.rs:1-24`, `examples/wallet-sequencer/src/bin/wallet-sequencer-devnet.rs:1-24`, `examples/c-wallet-engine/src/bin/c-wallet-genesis.rs:1-38`, `examples/c-wallet-engine/tests/genesis_cli.rs:1-17`, `examples/c-wallet-engine/src/lib.rs:420-427`

- Claim: There are three per-network genesis selections. The two wallet-sequencer binaries differ only at line 23 (`default()` vs `devnet()`). The guest has its own pair, canonical-app-devnet.rs and canonical-app-sepolia.rs. The C path uses c-wallet-genesis (21-29) plus --state-file (c-app-sequencer lib.rs:50-58). Freezing them is right: do not add a --preset flag. The deletion plan overreaches, though. The future tool consumes a *supplied* checkpoint, and something app-specific still has to produce the genesis checkpoint. c-wallet-genesis.rs:4-5 and c-wallet-engine lib.rs:422-427 state that the seam has no create path, so every app ships a genesis tool. The conformance fixture also uses write_genesis (conformance.rs:21). So c-wallet-genesis and write_genesis survive any version of the direction. At most, the Rust devnet binary and the run_main/run_command genesis-closure plumbing (about 30-40 lines) would go, and only if setup becomes dump-only. The guest pair cannot go, because the machine image bakes its config in at build time.
- Fix: Freeze. Do not add a `--preset` flag to wallet-sequencer or polish the genesis tool. When the shared tool lands, make setup consume a tool-produced startup dump for both Rust and C apps. That deletes wallet-sequencer-devnet, c-wallet-genesis plus its CLI test, and the genesis closure plumbing.
- Decision: Should genesis become just 'a checkpoint produced by the shared tool' (one startup-state input for setup), and should that tool bind the app's deployment preset to the chain id setup pins?

### J3. `--state-file` / CARTESI_SEQUENCER_STATE_FILE names a dump prefix (a directory), not the state file

`low · confirmed · +0 lines · S · config`  
Where: `bindings/c-app-sequencer/src/lib.rs:27-30`, `bindings/c-app-sequencer/src/lib.rs:57`, `tests/harness/src/sequencer.rs:1398`, `tests/harness/src/sequencer.rs:1448`, `bindings/c-app-engine/README.md:43-44`

- Claim: `--state-file` / CARTESI_SEQUENCER_STATE_FILE (c-app-sequencer lib.rs:27-30) is passed to `EngineApp::from_dump` (lib.rs:57), so it names a dump prefix, while `state_file_in_dump` means the canonical file inside a dump (sequencer-core application/mod.rs:252-256; wallet.rs:389-391). The harness ends up with <tmp>/state/state (sequencer.rs:1398, 1448), and the README repeats the usage (bindings/c-app-engine/README.md:43-44). Because the genesis tool survives the future direction (see J2), the rename does not need to wait for that tool.
- Fix: Rename to `--genesis-dump` / CARTESI_SEQUENCER_GENESIS_DUMP in the same patch that lands J2's tool. Do not rename it on its own before then.
- Decision: Rename now, or fold into the J2 startup-tool change?

### J4. Tracing init copied into every app binary; harness doc example calls a nonexistent init_tracing()

`low · partially · -22 lines · S · public-rust-api`  
Where: `examples/wallet-sequencer/src/main.rs:10-19`, `examples/wallet-sequencer/src/bin/wallet-sequencer-devnet.rs:10-19`, `bindings/c-app-sequencer/src/lib.rs:43-48`, `sequencer/src/harness.rs:7-18`

- Claim: The ANSI-on-TTY tracing block appears three times: wallet-sequencer main.rs:14-19, the devnet bin at 14-19, and c-app-sequencer lib.rs:43-48. The comment appears twice. The harness.rs:12-17 example calls an `init_tracing()` that exists nowhere; git grep finds only the doc line, and the block is `ignore`d, so it never compiles. The proposal is more costly than stated. The sequencer library has no tracing-subscriber dependency (only examples/wallet-sequencer/Cargo.toml:17 and bindings/c-app-sequencer/Cargo.toml:21 have one), so exporting init_tracing from the library adds a dependency and a process-global subscriber policy. Calling it inside run_command would double-initialize for c-app-sequencer, which already calls `.init()`, and `.init()` panics when a global subscriber is already set. Smallest fix: make the doc example show the real block, and, if wanted, add `harness::init_tracing` only under run_main's binary-facing path. Net about -10 lines, not -22.
- Fix: Export `sequencer::harness::init_tracing()`, or call it inside run_main/run_command after parsing (as c-app-sequencer already orders it). The Rust app's main then shrinks to one line, and the doc example becomes true.

### J5. Deterministic app state uses HashMap; canonical order depends on remembering to sort in the encoder

`low · partially · -45 lines · S`  
Where: `examples/app-core/src/application/wallet.rs:57-63`, `examples/app-core/src/wallet_snapshot.rs:46-63`, `examples/app-core/src/wallet_snapshot.rs:83-102`, `examples/app-core/src/application/wallet.rs:996-1036`

- Claim: WalletApp keeps HashMaps (wallet.rs:60-61), and canonical bytes depend on the sorts at wallet_snapshot.rs:54 and :63. A BTreeMap swap preserves the bytes: Address is FixedBytes<[u8;20]>, whose derived Ord is the same byte-lexicographic order as the current sort key. It also removes the sorts and both duplicate checks. Three corrections. (1) The golden vector (wallet_snapshot.rs:139-143) encodes the EMPTY wallet, so it does not guard ordering; byte equality has to be argued, or pinned with a non-empty vector. (2) The scrambled-order test (wallet.rs:996-1036) is vacuous today. It dumps the same unmutated HashMap twice, and an unmutated HashMap iterates in the same order both times, so the test would pass with the sort removed. That strengthens the case for deleting it. (3) Requiring strictly increasing addresses in decode tightens what decode accepts: unique but unsorted entries would be rejected. No encoder writes such bytes, but it is still an acceptance change for corrupt inputs. The 'RandomState seeds from randomness in the guest' point does not matter, because no unsorted iteration reaches output.
- Fix: Switch to BTreeMap<Address, _>. Address's Ord is byte-lexicographic, which matches the current sort key, so the snapshot bytes do not change (the golden vector at wallet_snapshot.rs:139-143 guards this). Drop the sorts. In decode, require strictly increasing addresses, which replaces both duplicate checks and makes decode∘encode a bijection. Delete the scrambled-order test.

### J6. wallet.rs test module duplicates sequencer-core guard tests and carries trivia, while the register's business-failure gap stays open

`medium · confirmed · -170 lines · S`  
Where: `examples/app-core/src/application/wallet.rs:420-479`, `examples/app-core/src/application/wallet.rs:541-549`, `examples/app-core/src/application/wallet.rs:551-609`, `examples/app-core/src/application/wallet.rs:812-823`, `examples/app-core/src/application/wallet.rs:856-881`

- Claim: sequencer-core already pins the max-fee and exhausted-nonce guards generically (sequencer-core/src/application/mod.rs:512-523, 526-547), so wallet.rs:420-479 duplicates them. The Legacy* SSZ types (551-609) exist only to test an unknown union selector; Method has two variants (method.rs:11-13), and conformance.rs:94 already runs `vec![0xff]`. A one-line unknown-selector payload in an existing wallet test keeps the balance-unchanged assertion that conformance (which only compares native with bridge) lacks. The trivia tests are 541-549, 812-823 and 1057-1065. 856-881 nearly duplicates 826-854. No test overdraws after the fee: conformance's mixed history is fully funded (conformance.rs:53-94). Removing ~174 lines and adding a ~25-line business-failure test nets about -150, not -170.
- Fix: Delete the listed tests. Replace the legacy block with a one-line malformed-method case. Add one test that pins an insufficient transfer and an insufficient withdrawal: fee, nonce and progress advance, and there are no outputs and no other balance change.

### J8. WalletConfig doc says the fee recipient defaults to Address::ZERO; Default is Sepolia's sequencer address

`low · confirmed · -1 lines · S · docs-only`  
Where: `examples/app-core/src/application/wallet.rs:29-30`, `examples/app-core/src/application/wallet.rs:34-39`, `examples/app-core/src/application/wallet.rs:51-55`, `examples/app-core/src/application/wallet.rs:856-881`

- Claim: wallet.rs:29 says the fee recipient 'Defaults to Address::ZERO (burned)'. But Default is sepolia() (51-55), whose sequencer_address is SEPOLIA_SEQUENCER_ADDRESS (38, 73-74). The test 'fee_sent_to_zero_address_when_default' (856-881) sets ZERO explicitly. See JS1 for the root cause.
- Fix: Change the doc to 'Address credited with fee revenue.' Delete or rename the test (covered under J6).

### J9. Engine status mapping drops PermissionDenied, so the host's PermissionDenied-terminal arm can never fire for C apps

`low · confirmed · -1 lines · S · BC`  
Where: `examples/c-wallet-engine/src/lib.rs:51-70`, `bindings/c-app-engine/src/lib.rs:78-84`, `sequencer/src/commands/error.rs:159-173`, `sequencer/src/ingress/inclusion_lane/dump_info.rs:27-35`

- Claim: On AppBootstrap, the host classifies Io(PermissionDenied) as terminal (sequencer/src/commands/error.rs:159-172). Over the C seam, the reference engine maps it to IO_ERROR (c-wallet-engine lib.rs:52-63), and the shim turns that into ErrorKind::Other (c-app-engine lib.rs:78-79), which is classified Unclassified (error.rs:186), giving exit 1. The arm is also reachable natively beyond genesis reads: write_baseline_dump maps any create_dump AppError into AppBootstrap (setup/fill.rs:70-72), so an unwritable dumps dir exits 30 natively and 1 over C. error.rs:163 also contradicts the rationale for referenced artifacts (dump_info.rs:24-34), which keeps permission failures operational because they 'may clear'. Dropping PermissionDenied from error.rs:163 makes both paths and both policies consistent.
- Fix: Frequency is low (operator misconfiguration only) and the fallback is safe (a retried failure that keeps failing), so add no new status. Drop PermissionDenied from error.rs:163 so native and C agree it is operational, or document the asymmetry in the header. Point the engine's mapping comment at referenced_artifact_io_is_terminal.
- Decision: Should PermissionDenied on the genesis dump be terminal (exit 30) or operational (exit 1)?

### J10. Dead public surface and a handled-impossible branch in the example crates

`nit · unverified-nit · -25 lines · S · public-rust-api`  
Where: `examples/app-core/src/application/mod.rs:14-16`, `examples/app-core/src/wallet_snapshot.rs:22-43`, `examples/app-core/src/application/wallet.rs:159-161`, `examples/app-core/src/application/wallet.rs:204-213`, `examples/c-wallet-engine/src/lib.rs:408-417`

- Claim: The re-export aliases `encode_wallet_snapshot`/`decode_wallet_snapshot` have no users. The Snapshot* structs are `pub` but used only inside their module. The inherent `last_executed_safe_block` shadows the trait's default method. The interior-NUL branch in `state_file_in_dump` cannot be reached, because the prefix came from a CStr and `join("state")` adds no NUL. AGENTS.md says impossible states fail loud rather than being handled.
- Fix: Delete the aliases and the inherent method, make the Snapshot* structs private, inline decode_portal_erc20_deposit, and replace the NUL branch with `.expect("prefix came from a CStr")`.

### J11. ERC-20 deposit encoding and EIP-712 user-op signing hand-rolled in many test files

`low · confirmed · -70 lines · S`  
Where: `examples/app-core/src/application/wallet.rs:611-617`, `examples/canonical-test/src/main.rs:430-436`, `examples/c-wallet-engine/tests/conformance.rs:32-42`, `examples/canonical-test/src/main.rs:334-368`, `sdk/guest/rollups-types/src/lib.rs:220-236`

- Claim: rollups-types has decode only (sdk/guest/rollups-types/src/lib.rs:220-235). The three deposit encoders are at wallet.rs:611-617, canonical-test main.rs:430-436 and conformance.rs:32-42. Nine parity-search signing loops exist, all in test code (canonical-test:352, sequencer-core scheduler/mod.rs:657, api.rs:520/:567 inside its tests module at :271, e2e_sequencer.rs:74/124/166/1595, harness wallet.rs:335). A cross-crate shared helper needs a feature-gated home. The simpler route is to switch each site to k256 `sign_prehash_recoverable` (or alloy's signer) in place, with no new shared module. Most sites are outside area J.
- Fix: Add `Erc20Deposit::encode` next to decode, which also gives decode a round-trip test. Add one shared test helper `sign_user_op(key, op, domain)` that uses k256 `sign_prehash_recoverable` to get the parity directly. Migrate the in-area copies first.

### J12. canonical-app justfile duplicates every devnet/sepolia recipe and shares their intermediate outputs

`low · partially · -30 lines · S`  
Where: `examples/canonical-app/justfile:17-23`, `examples/canonical-app/justfile:41-51`, `examples/canonical-app/justfile:55-71`, `examples/canonical-app/justfile:82-104`

- Claim: The build-dapp-*, build-rootfs-* and build-machine-image* recipe pairs (justfile:41-51, 55-71, 82-104) differ only in network name, so parameterizing them saves about 25 lines. Sharing the intermediates (dapp, rootfs tar/ext2, lines 17-20) causes no stale mix: every image recipe re-runs its own dapp and rootfs dependencies in order (82→55→41, 94→64→47). Per-net intermediates are therefore not needed. The recipe names are referenced by the root justfile (68-71, 89-93), .github/workflows/release.yml:148-151 and the watchdog docs, so keep them as aliases.
- Fix: Parameterise with `net := devnet|sepolia` (for example `build-machine-image net="devnet"`), use per-net intermediate paths, and keep the two image output names.

### J13. Error messages without context, Debug formatting, and a user-triggerable warn repeated on every re-execution

`nit · partially · +0 lines · S`  
Where: `examples/app-core/src/application/wallet.rs:266-269`, `examples/c-wallet-engine/src/lib.rs:218-220`, `examples/c-wallet-engine/src/lib.rs:66`, `examples/c-wallet-engine/src/bin/c-wallet-genesis.rs:33`, `examples/app-core/src/application/wallet.rs:327-334`

- Claim: These are minor message cleanups. wallet.rs:267-268 is an impossible-state error (the fee was validated first; conformance.rs:236 calls it an invariant fault), so adding context there is fine. engine lib.rs:218-219 is documented as unreachable (lib.rs:211-213). `{other:?}` at engine lib.rs:66 and `{err:?}` at c-wallet-genesis.rs:33 could use AppError's Display (sequencer-core application/mod.rs:13-20). Do NOT downgrade the unsupported-token warn at wallet.rs:328. Such a deposit moves the tokens to the app contract and credits nobody, so it records user funds stuck with no refund path. Repetition is bounded by attacker L1 gas per deposit, plus replays after a restart or rebuild.
- Fix: Add sender, fee and balance to the Internal reason, name the reason in the engine message, use `{err}`, and downgrade the unsupported-token log to debug, or info at most.

### J14. Comments that restate the trait contract or carry cross-module lore already enforced elsewhere

`nit · unverified-nit · -15 lines · S · docs-only`  
Where: `examples/app-core/src/application/wallet.rs:373-378`, `examples/app-core/src/application/wallet.rs:75-84`, `examples/app-core/src/application/wallet.rs:241-242`, `examples/c-wallet-engine/src/lib.rs:229-232`

- Claim: Several comments restate the trait's durability doc, carry sync obligations the harness already asserts, or are hard to parse.
- Fix: Replace each with a one-line pointer: 'durability: see Application::create_dump', and 'anvil #9, distinct from the deployer (setup's nonce gate); harness asserts sync'. Delete 241-242 and reword 229-232 as 'refuse while outputs are queued: overwriting would drop them'.

### JS1. `Default` on WalletConfig/WalletApp silently selects Sepolia; the release binary and the genesis tool pick a network implicitly

`low · skeptic-added · -8 lines · S · public-rust-api`  
Where: `examples/app-core/src/application/wallet.rs:51-55`, `examples/app-core/src/application/wallet.rs:216-220`, `examples/wallet-sequencer/src/main.rs:23`, `examples/c-wallet-engine/src/bin/c-wallet-genesis.rs:24`, `examples/app-core/src/application/wallet.rs:29`

- Claim: A deployment-specific config implements Default as `sepolia()`. As a result, the release wallet-sequencer's network (main.rs:23) and c-wallet-genesis's no-argument fallback (c-wallet-genesis.rs:24) are chosen implicitly, and the field doc wrongly says the default fee recipient is ZERO (J8). An operator or app author has to read the Default impl to learn which network a binary serves. A sequencer/guest config mismatch shows up as canonical divergence (operator-deployment.md:315 pairs the release binary with the Sepolia image).
- Fix: Delete both Default impls. Call `WalletConfig::sepolia()` explicitly in main.rs:23 and in tests, and make c-wallet-genesis require the preset argument. The golden vector bytes do not change, because the vector is sepolia().
- Decision: Should c-wallet-genesis require the preset (a CLI change) or keep Sepolia as an explicit, documented fallback?

Refuted:

- J7. Dump-contract tests are hand-written per app; no reusable check for an app author's Application impl. The duplication evidence is inaccurate, the -60 estimate ignores the helper's own size, and this conflicts with 'quantify before machinery' and Track 6 ownership.

Keep (load-bearing):

- c-wallet-engine delegates every entry point to WalletApp (examples/c-wallet-engine/src/lib.rs:139, 188, 276, 300, 391) rather than reimplementing the rules. That gives zero business-logic duplication between the Rust and C wallets, and the native-vs-bridge conformance suite (conformance.rs:44-161) is the seam's only end-to-end proof.
- WalletApp::create_dump's create_dir (not create_dir_all) plus fsync of the file, the prefix and its parent (wallet.rs:367-385). This is the I13 obligation stated at sequencer-core/src/application/mod.rs:234-241; shorten the comment but keep the code.
- from_snapshot_parts returns a typed AppError on incoherent progress instead of panicking (wallet.rs:97-118). Dump bytes are untrusted input.
- The golden empty-snapshot vector test (wallet_snapshot.rs:139-143) is shared with the Lua watchdog (watchdog/tests/e2e.lua:163). It also guards J5's byte-identity.
- `use c_wallet_engine as _;` in c-wallet-sequencer main.rs:9-11 is a load-bearing link import, as its comment says.
- c-app-sequencer build.rs uses a cfg rather than a cargo feature (build.rs:6-13), so `--all-features` cannot produce a binary with no engine to link.
- The engine's execute() refuses to run while outputs are still queued (c-wallet-engine lib.rs:240-243). Otherwise chain-bound outputs would be silently dropped.
- The canonical-test fee-arithmetic test (canonical-test main.rs:224-302) is the only guest-side test with a nonzero fee, and it pins guest gas == host fee_to_linear from both sides.
- The scheduler shell's inspect path reports and keeps serving instead of panicking (canonical-app scheduler/mod.rs:59-73). Inspect is a public read-only endpoint.
- DEVNET_SEQUENCER_ADDRESS being Anvil account 9, distinct from the deployer (wallet.rs:85-86). A past drift between account 0 and account 9 caused CI reds; keep the fact even after J14 shortens the comment.

## K. Tests, harness, benchmarks

Verdict: Area K holds three test layers. Crate-internal unit and integration tests (sibling test files about 7.3k lines, src/integration_tests about 4.7k lines, plus inline modules) sit in front of three process-level crates under tests/: harness about 3.7k, e2e about 5.2k with 49 libtest-mimic scenarios, and benchmarks about 4.1k. Coverage depth fits a security-critical sequencer. The size does not: fixtures are copied rather than shared. There are 10 hand-written Application stubs, two identical TempDb helpers in three differently named helper modules, four bespoke in-process server bootstraps, two batch-tree invariant checkers, and around 140 repeated open/append/timing boilerplate blocks in recovery_tests.rs alone. Together that is roughly 1.3k removable lines with no loss of discrimination. Correctness problems in the tests themselves: - 43 integration tests silently pass when the listener cannot bind. - The 429 test relies on a 50 ms sleep and can hang instead of failing. - Two e2e scenarios sleep a fixed 7 s instead of polling. - e2e hand-copies sequencer defaults, citing a file that does not exist. The benchmark crate is heavier than the questions it answers. Its target-verdict code can only ever print NOT_EVALUATED. compare_latest re-parses CSV and re-implements the capacity summary the library already computes. The sweep bin has two copy-pasted branches. Roughly 3.5k test lines protect cross-rebuild client continuity and cockroach rebuild. The new direction (a shared recovery tool that produces both startup states) replaces that machinery, so freeze those tests rather than polish them. Worth keeping: the outage matrix (TcpProxy and faketime), the post-scenario schema check against the real binary's DB, the schema-trigger tests, the boundary tests for each recovery entry point, the C-host scenario variants, and kill_on_drop teardown.


### K1. 43 in-crate integration tests silently pass when the test listener cannot bind

`medium · confirmed · -170 lines · S`  
Where: `sequencer/src/integration_tests/e2e_sequencer.rs:1183-1192`, `sequencer/src/integration_tests/e2e_sequencer.rs:1290-1296`, `sequencer/src/integration_tests/ws_broadcaster.rs:394-403`, `sequencer/src/integration_tests/snapshot_endpoints.rs:47-55`, `sequencer/src/integration_tests/e2e_sequencer.rs:207-209`

- Claim: Confirmed as stated. Four helpers map bind PermissionDenied to eprintln + None: e2e_sequencer.rs:1184-1192 and :1290-1296, ws_broadcaster.rs:394-403, snapshot_endpoints.rs:49-55. There are 43 `else { return; }` sites under src/integration_tests. The risk is specific to this repo: agents run `cargo test` inside sandboxes that can deny bind, so a green suite there can hide 43 tests that never ran. Fix: fail on bind error. If sandboxes must stay supported, require an explicit env opt-in for the skip; do not keep the silent pass. Realistic delta is about -150.
- Fix: Panic on any bind error (or `expect`), make the helpers return the runtime directly, delete the let-else at all 43 sites. If a sandbox genuinely cannot bind, gate the module with #[ignore] and an explicit env flag instead of a silent pass.
- Independent trace: confirmed. The code matches the claim exactly. Only PermissionDenied takes the silent path;

### K2. Three fixture modules under three names, duplicated TempDb, four bespoke in-process server bootstraps

`medium · partially · -250 lines · M`  
Where: `sequencer/src/integration_tests/common.rs:8-23`, `sequencer/src/storage/test_helpers.rs:28-43`, `sequencer/src/commands/test_support.rs:1-89`, `sequencer/src/integration_tests/e2e_sequencer.rs:1179-1282`, `sequencer/src/integration_tests/e2e_sequencer.rs:1284-1337`

- Claim: Real duplication: - TestDb/temp_db is byte-identical in integration_tests/common.rs:8-23 and storage/test_helpers.rs:28-43. The integration tests are in-crate (lib.rs:33-35) and could import crate::storage::test_helpers (storage/mod.rs:48 is pub(crate)), so common.rs can be deleted. - wait_until is duplicated at inclusion_lane/tests.rs:1000 and setup/fill.rs:593. Overstated: only start_full_server_with_max_body does genesis-dump registration and ensure_open_tip (e2e_sequencer.rs:1201-1225). The other three bootstraps share only bind, L2TxFeed::new and start_on_listener with SnapshotState, about 25 lines each. They differ on purpose: a parked queue (:1299-1336), ws_max_subscribers (ws_broadcaster.rs:418-424), and a dummy state layout (ws_broadcaster.rs:382-384). A single TestServer builder saves less than claimed: realistic total is about -100 to -130, not -250.
- Fix: One #[cfg(test)] crate::test_support module (submodules if needed) holding temp_db, wait_until, pin_test_deployment_identity, the stub app (K3) and a single TestServer builder (with_lane / parked_queue(cap) / max_body). Delete common.rs.

### K3. Ten hand-written Application stubs repeat the same dump/from_dump/state_file plumbing

`medium · partially · -350 lines · M`  
Where: `sequencer/src/ingress/inclusion_lane/tests.rs:55-128`, `sequencer/src/ingress/inclusion_lane/tests.rs:130-186`, `sequencer/src/ingress/inclusion_lane/tests.rs:201-356`, `sequencer/src/ingress/inclusion_lane/tests.rs:1770`, `sequencer/src/commands/test_support.rs:22-74`

- Claim: Confirmed: - There are 10 test `impl Application`. One, dump_info.rs:271 PrefixDumpApp, is missing from the locations. - TestApp.nonces is write-only (tests.rs:57, 91-93). - The SweepTestApp const hack is at test_support.rs:27-31. - SharedCountingApp, UserOpCounterApp and CountedSweepTestApp are the same progress-persisting stub. fill.rs:137-151 even re-implements decode_progress inline. The proposed single StubApp with instance knobs does not fit the trait. from_dump is static, and the lane reloads through it, so instance knobs survive only on directly constructed instances (tests.rs:533, :1978). - TestApp, InternalUserOpApp, SweepTestApp and StartupProbeApp reset to default on reload (tests.rs:113-115, workers.rs:722-726). - The counting stubs decode the persisted progress. - ReplayForbiddenApp and PrefixDumpApp (const-generic layout) must stay bespoke. Making every stub persist progress would change reload semantics for the reset-style stubs. Realistic shape: one persisting counter stub, one reset stub with knobs, plus the two bespoke ones. That is about -200, not -350.
- Fix: One StubApp in test_support. It persists progress via encode/decode_progress and has opt-in knobs (reject_after, fail_dump, fail_validation, fail_apply, record: Option<Vec<ReplayEvent>>). Keep a bespoke impl only where the static from_dump must behave differently (ReplayForbiddenApp). Drop the write-only nonce map.

### K4. recovery_tests.rs: ~140 repeated open/append/timing blocks, plus 680 lines of schema tests in the wrong file

`medium · confirmed · -500 lines · M`  
Where: `sequencer/src/storage/recovery_tests.rs:26-56`, `sequencer/src/storage/recovery_tests.rs:1913-1938`, `sequencer/src/storage/recovery_tests.rs:2136-2818`, `sequencer/src/storage/recovery.rs:642-643`

- Claim: Confirmed, with corrected counts: - `Storage::open(db.path.as_str())` appears 68 times. - `&default_protocol_timing()` appears 57 times, not 65. - `StoredSafeInput {` appears 38 times. schema_invariants spans recovery_tests.rs:2136-2818 (682 lines). recovery.rs:641-643 is the only #[path] module in sequencer/src. The schema module's own comment (recovery_tests.rs:2141-2148) says it lives there only to share fixtures. Those fixtures are already in test_helpers, so moving it is a pure relocation. Realistic delta is about -350 to -450. The appends vary in sender, payload and count, so not all of them collapse to one line.
- Fix: Extend the fixture trait in test_helpers with land(block, &[(sender, payload)]) and advance_safe_head(block), using SENDER_A and the default timing. Add a fixture fn that returns an opened Storage. Move schema_invariants next to the migration code (storage/tests.rs or open.rs tests). Keep every scenario.

### K5. About 3.5k test lines guard cross-rebuild client continuity and the in-process cockroach rebuild: freeze them

`medium · partially · +0 lines · S`  
Where: `sequencer/src/integration_tests/historical_bootstrap.rs:1-561`, `sequencer/src/integration_tests/historical_bootstrap/recovery_compatibility.rs:1-403`, `sequencer/src/storage/history/generation_tests.rs:1-170`, `tests/e2e/src/cold_replica.rs:1-366`, `sequencer/src/commands/setup/checkpoint_tests.rs:1-226`

- Claim: The superseded set is much smaller than 3.5k lines. Recovery generations and cuts are advanced by the automatic stale-batch cascade (recovery.rs:487-493, cascade_and_reopen), not only by cockroach rebuild. These tests therefore guard client continuity across routine automatic recovery, which the recovery tool does not replace: - generation_tests.rs (170) - canonical/tests.rs (429) - cold_replica.rs (366). Its "process-boundary recovery" is advance_wall_and_mine past MAX_WAIT plus respawn (cold_replica.rs:184-190), not a rebuild. - recovery_compatibility.rs ('survives missed recoveries') At most they are frozen pending the later sync redesign, not slated for deletion when the tool lands. The snapshot download tests are not superseded. /finalized_state, /inclusion_block and /digest are consumed by the watchdog (watchdog/sequencer.lua:48-64). The LeaseReadError scaffolding (snapshot_endpoints.rs:276-355) backs the /finalized_state lease tests (:357-638). checkpoint_tests.rs pins rebuild_from_checkpoint refusal safety. That logic moves into the tool rather than dying, so freeze it and migrate it. Genuinely superseded by the tool: the cross-rebuild client path, i.e. historical_bootstrap.rs (561, 'crosses the rebuilt baseline', LifecycleCommand::Rebuild at :270) and the era/raw-history egress tests in historical/tests.rs (497). Total is about 1.1k lines.
- Fix: Freeze: do not apply K2-K4 consolidation here beyond mechanical path fixes. Delete or move these tests together with the machinery when the recovery tool lands. Its tests should become checkpoint-to-(sequencer DB, client DB) golden tests. The snapshot lease-read retry scaffolding (LeaseReadError, :276-345) goes with it.
- Decision: Confirm snapshot download (snapshot_endpoints.rs) is in the superseded set, not kept for the watchdog or operator tooling.

### K6. Benchmark target-verdict code can only ever say NOT_EVALUATED; the spec lists harnesses that do not exist

`medium · confirmed · -280 lines · S`  
Where: `tests/benchmarks/src/evaluation.rs:13-214`, `tests/benchmarks/src/bin/round_trip_latency.rs:72,199-206`, `tests/benchmarks/src/bin/sweep.rs:93,391`, `tests/benchmarks/BENCHMARK_SPEC.md:83-139`, `tests/benchmarks/README.md:63`

- Claim: Confirmed. NetworkProfile::same_host_baseline() is the only constructor used (round_trip_latency.rs:72, sweep.rs:93). CanonicalNetworkAware is never built, so every verdict is NOT_EVALUATED (evaluation.rs:39, 171-187). Section 7 of the spec maps metrics to `ack_latency` and `unit_hot_path`, but src/bin holds only compare_latest, report, round_trip_latency and sweep. When deleting, keep a one-line network-profile string in the report, because spec section 6.5 requires it.
- Fix: Delete evaluation.rs, the --evaluate flags and the verdict JSON field. Shrink the spec to the two p99 targets and the sample-size rule, and fix section 7. Re-add a verdict only when a network shaper exists.

### K7. Benchmark tooling duplicates itself: CSV re-parsing, a re-implemented capacity summary, copy-pasted sweep branches, a redundant bin

`medium · partially · -700 lines · M`  
Where: `tests/benchmarks/src/bin/compare_latest.rs:35-57,283-390,431-534`, `tests/benchmarks/src/sweep.rs:110-145`, `tests/benchmarks/src/bin/sweep.rs:236-330`, `tests/benchmarks/src/bin/sweep.rs:340-450`, `tests/benchmarks/src/bin/round_trip_latency.rs:1-238`

- Claim: Confirmed: - compute_sweep_summary (compare_latest.rs:509-533) re-implements compute_capacity_summary (sweep.rs:110-145) field for field, over hand-parsed CSV rows (compare_latest.rs:431-500, 351-390). - The sweep bin writes JSON in both branches (:353, :481). - report.rs already loads that JSON (report.rs:26-36, 258-297). - The ack and round-trip branches in bin/sweep.rs share their structure. Not verified: that round_trip_latency is exactly `sweep --round-trip` at one concurrency. It owns the --evaluate soft-confirm path (round_trip_latency.rs:199-206), which disappears with K6, and possibly other flags. Realistic delta is about -400 to -500, not -700.
- Fix: Make JSON the only machine-read artifact (CSV becomes print-only or goes away). Use one row type with optional round-trip columns, one sweep code path that takes a Mode, and drop round_trip_latency (just keeps the recipe name). compare_latest then loads two JSONs and diffs the library summaries. Goal: about 4.1k down to about 3.3k lines. Do this before K8 so the new metrics land once.
- Decision: Whether the ack-only sweep (no WS clients) must stay a separate mode for capacity runs. Recommended: keep it as a flag, not a separate code path.

### K8. N17: add DB size and post-load batch drain time to the benchmark report

`low · partially · +120 lines · S`  
Where: `tests/benchmarks/src/runtime.rs:107-217`, `tests/benchmarks/src/bin/round_trip_latency.rs:172-193`, `tests/harness/src/sequencer.rs:571-591`, `tests/harness/src/sequencer.rs:700-731`, `tests/harness/src/sequencer.rs:876`

- Claim: The gap is real. MemoryReport (runtime.rs:107-116) is the only resource report, and the harness has the building blocks: data_dir (:876), count_safe_accepted_batches (:571-591) and mine_live_l1_blocks (:938-940). StorageReport (DB plus WAL bytes) is cheap and well defined. drain_secs is less meaningful than claimed. In self-contained mode it is shaped by: - harness-driven mining at 1 s per block (rollups.rs:95-102) - the Anvil safe-head lag (--slots-in-an-epoch, rollups.rs:206) - the submitter idle poll of 5 s by default (test_cases.rs:3074-3076) So it measures poll cadence times batch count, not submitter throughput. Report it as an environment-shaped profiling number, or ship only StorageReport first. This is a missing metric, not a defect, so severity is low.
- Fix: (a) StorageReport { db_bytes, wal_bytes, bytes_per_1k_accepted } from a stat of data_dir/sequencer.db and -wal when the run ends, recorded next to MemoryReport (self-contained only). (b) When the last load worker returns, record t0 and the highest sealed-batch nonce (add a MAX(nonce) accessor beside count_safe_accepted_batches). Then loop mine_live_l1_blocks(1) and poll MAX(nonce) in safe_accepted_batches until it reaches that nonce, with a deadline. Report drain_secs and batches_drained. Exclude the open tip so the metric measures submitter throughput, not max_batch_open. About 120 lines, no machinery.
- Decision: Whether 'after all workers stop' means the load generators stop (recommended; measures the submitter backlog) or the sequencer's own workers shut down (that would measure flush-mempool instead).

### K9. Timing-based tests: the 50 ms race in the 429 test can hang; fixed 7 s submitter sleeps in e2e

`low · confirmed · -10 lines · S`  
Where: `sequencer/src/integration_tests/e2e_sequencer.rs:464-472`, `sequencer/src/integration_tests/e2e_sequencer.rs:1301,1331-1337`, `sequencer/src/integration_tests/e2e_sequencer.rs:1546-1567`, `tests/e2e/src/test_cases.rs:3077,3120`, `tests/e2e/src/test_cases.rs:3909,3944`

- Claim: Confirmed. /tx does try_send and then awaits a oneshot (ingress/api.rs:257-268). With the receiver parked (e2e_sequencer.rs:1336) and no timeout in post_raw_json (:1546-1567), the 429 test hangs if the main request wins the single slot. Both e2e scenarios sleep a fixed 7 s (test_cases.rs:3077/3120, 3909/3944). Better fix for the 429 case: add a deterministic unit test in ingress/api.rs with a pre-filled channel(1), modelled on closed_lane_is_service_unavailable (api.rs:289), and drop the racy integration test.
- Fix: Fill the parked queue directly (keep a tx clone and try_send a dummy PendingUserOp) and drop the sleep and the spawned first request. In e2e, poll Anvil's pending tx count (txpool) with a deadline instead of the 7 s sleep, and hoist the shared constant.

### K10. Two hand-written batch-tree invariant checkers that disagree on the anchor

`low · confirmed · -90 lines · S`  
Where: `sequencer/src/storage/recovery_tests.rs:2835-2924`, `tests/harness/src/sequencer.rs:752-858`, `tests/e2e/src/main.rs:45-49`, `sequencer/src/storage/migrations/0001_schema.sql:110`

- Claim: Confirmed. The crate copy asserts that a NULL parent has nonce 0 (recovery_tests.rs:2853-2856). The harness copy is anchor-aware (harness sequencer.rs:760-767) and also checks the single valid root. Both run the same LEFT JOIN query.
- Fix: Keep the harness check: it is the only check against the real binary's DB after each scenario. Drop the crate copy and have tree_invariants_hold_across_mixed_workload rely on the trigger tests, or move one anchor-aware checker into test_support and call it from both.

### K11. e2e hand-copies sequencer defaults and cites a file that does not exist

`low · confirmed · +0 lines · S`  
Where: `tests/e2e/src/test_cases.rs:72-80`, `sequencer/src/commands/config.rs:51-52`

- Claim: Confirmed. test_cases.rs:72-80 cites runtime/config.rs, which does not exist (sequencer/src/runtime holds only mod.rs, process_lock.rs and shutdown.rs). The real default is commands/config.rs:49-53 (default_value = "300"). ManagedSequencerConfig would need a new field or env passthrough so the harness can set these values explicitly.
- Fix: Have the harness spawn config set CARTESI_SEQUENCER_PREEMPTIVE_MARGIN_BLOCKS and SECONDS_PER_BLOCK explicitly from these constants, so the test owns the values instead of copying them. Drop the sync comment.

### K12. e2e scenario configuration is keyed by name strings

`nit · confirmed · +0 lines · S`  
Where: `tests/e2e/src/main.rs:19-34`

- Claim: Confirmed: main.rs:19-33 selects the binary by the c_host_ prefix, disables faketime for three literal names, and sets the fee oracle for one name. This is a style issue, and it is low-risk because names are rarely renamed.
- Fix: Have test_cases() return (name, run, configure: fn(&mut ManagedSequencerConfig)) so each scenario declares its own config next to its body.

### K13. Harness errors leave out the context needed to diagnose flakes; skip logs hide vacuous passes

`nit · partially · +5 lines · S`  
Where: `tests/harness/src/ws.rs:31-34`, `tests/harness/src/util.rs:57-60`, `tests/harness/src/sequencer.rs:1616-1621`, `sequencer/src/integration_tests/snapshot_endpoints.rs:52`

- Claim: Real but small: - The WS timeout error 'wait for websocket frame' (ws.rs:31-34) omits the 10 s timeout and the expectation. - 'sequencer exited before readiness' (util.rs:57-60) omits the log path. The link to the register entry is overstated. The recorded failure was a 'Connection reset', which already reaches the error text via `receive websocket frame: {err}` (ws.rs:35). The register asks for server-side logs, which CI now uploads (register.md:147-158).
- Fix: Include the timeout and the expectation in the WS error (expect_user_op_from can pass the sender), and append log_path() to readiness and exit errors. The skip logs go away with K1.

### K14. Test comments that restate code, describe their callers, or have gone stale

`nit · unverified-nit · -30 lines · S`  
Where: `tests/harness/src/sequencer.rs:565-570`, `sequencer/src/integration_tests/e2e_sequencer.rs:1306-1311`, `sequencer/src/ingress/inclusion_lane/tests.rs:107-111,416-418`, `sequencer/src/ingress/inclusion_lane/tests.rs:206-211`, `tests/harness/src/sequencer.rs:745-752`

- Claim: count_safe_accepted_batches' doc describes the one test that calls it. The 'Sentinel submitter: pass an address no fixture seeds' comment sits on an L2TxFeedConfig literal that no longer has an address field. The comments around drop(app) and from_dump restate the code. SharedCountingApp claims it 'counts direct-input executions' but only persists progress. assert_schema_invariants is documented as panicking while it returns HarnessResult (mixed error channels).
- Fix: Delete or trim them in the K2/K3 sweep. Per AGENTS.md, keep only comments that state a non-obvious why.

### K15. `extern crate self as sequencer` lets in-crate tests spell the same path two ways

`nit · unverified-nit · -2 lines · S`  
Where: `sequencer/src/lib.rs:32-35`, `sequencer/src/integration_tests/e2e_sequencer.rs:7,16-22`

- Claim: Left over from moving the tests into src (bc8ffe8). Integration files mix `crate::storage::...` and `sequencer::storage::...`; there are 35 `sequencer::` sites across five files.
- Fix: Rewrite sequencer:: to crate:: in integration_tests and drop the alias. A mechanical change.

### KS1. Harness repeats the read-only DB open block in ten accessors

`low · skeptic-added · -50 lines · S`  
Where: `tests/harness/src/sequencer.rs:387`, `tests/harness/src/sequencer.rs:446`, `tests/harness/src/sequencer.rs:468`, `tests/harness/src/sequencer.rs:553`, `tests/harness/src/sequencer.rs:572`

- Claim: Each harness DB accessor repeats the same 6-line block: join("sequencer.db"), open_with_flags(READ_ONLY) and the same map_err. The DB filename is spelled out 11 times.
- Fix: Add one private `fn open_db_read_only(&self) -> HarnessResult<rusqlite::Connection>` and call it from every accessor. This also gives a single home for the DB filename that K8's StorageReport needs.

### KS2. Stale 'Sentinel submitter' comments describe an argument L2TxFeed::new no longer takes

`nit · skeptic-added · -5 lines · S`  
Where: `sequencer/src/integration_tests/e2e_sequencer.rs:1253-1254`, `sequencer/src/integration_tests/e2e_sequencer.rs:1309-1310`, `sequencer/src/integration_tests/snapshot_endpoints.rs:60`

- Claim: These comments say the fixture passes a sentinel submitter address. L2TxFeed::new takes only (db_path, shutdown, config) (egress/l2_tx_feed/mod.rs:71), and the L2TxFeedConfig literals the comments sit in have no such field. The real sentinel is NO_OWN_BATCHES, pinned through the deployment identity in l2_tx_feed/tests.rs:322-339.
- Fix: Delete the three stale comments.

Keep (load-bearing):

- src/integration_tests inside the crate (integration_tests/mod.rs:1-2): keeps the raw worker-launch APIs crate-private; moving them to tests/ would force public surface.
- tests/harness TcpProxy (harness/src/proxy.rs, about 440 lines) together with faketime: the only way to drive the provider-outage / both-down / hiccup scenario matrix.
- Harness assert_schema_invariants run after every scenario (e2e/src/main.rs:45-49): the only structural check against the real binary's DB. Keep this anchor-aware copy (see K10).
- The schema_* trigger tests (recovery_tests.rs:2152-2801): the schema is the enforcement layer for the batch-tree invariants. Move them (N4), do not cut them.
- Boundary pairs that look redundant but cover different entry points: recover_aging_tip (recovery_tests.rs:985-1022) vs recover_post_flush (:1913-1966).
- The reject-flood frontier-starvation test (inclusion_lane/tests.rs:505-585): its sleep-based stub is the only pin on bounded turns versus divergence detection.
- The c_host_ scenario variants (e2e main.rs:20, test_cases.rs:160-171): they run the same scenarios through the C ABI bridge.
- Zone-math derived constants and compile-time asserts in test_cases.rs:57-100: they tie the outage scenarios to MAX_WAIT_BLOCKS (fix the copied defaults per K11, keep the derivation).
- Serial e2e runner (main.rs:12, test_threads=1): scenarios share Anvil, faketime and the process lock.
- kill_on_drop on spawned children (harness/sequencer.rs:1397,1565): a panicking assertion still tears down Anvil and the sequencer.
- MemorySampler and the accepted-tx-to-WS-event sanity assertion (BENCHMARK_SPEC section 3.5): the benchmark's real correctness guard and memory profile.

## L. Build, CI, supply chain, cross-compilation

Verdict: Build/CI/supply-chain is about 2,250 lines: 21 manifests, two workflows, three composite actions, three justfiles, six scripts, two Dockerfiles and one build.rs. Its essential complexity is real. A host workspace that links the emulator (testsi), a RISC-V guest cross-build whose template hash is consensus-relevant, a C-engine seam and a Lua/Docker watchdog all need pinned, hash-verified inputs. The core is sound and should be kept. Every downloaded artifact is SHA-pinned. The cross image is pinned by digest. CI runs with --locked. default-members keeps a plain `cargo build` free of the CM/libclang requirement. Clippy already covers all 21 members, guest crates included, through libcmt's host mock. The biggest problems sit around that core, not in it. (1) The release path's supply-chain posture is weaker than CI's: third-party actions are on mutable tags with workflow-wide write permissions, cross/just are unpinned, and a dispatch builds the dispatched branch while labelling it with an arbitrary tag. (2) CI runs every PR push twice with no concurrency cancel, and builds the devnet guest image in two jobs. (3) The pin plumbing (verify/load scripts, a Rust version pinned in three places) is about 150 lines that mostly compare the pins file with itself, while the real drift points go unchecked: Foundry version ×3, contracts version ×2, guest-tools version. (4) The lockfile looks heavier than the build: 537 entries, ark-ff×4, three syn majors. Most of that comes from weak `dep?/feat` optional entries that never compile. The compiled duplicates the manifests actually cause are four (reqwest, tokio-tungstenite, thiserror, toml), plus one unused direct dep. Measure with `cargo build --timings` / `cargo tree -d -e normal,build` before any wider dependency surgery.


### L1. Release path: mutable action tags, workflow-wide write token, unpinned cross/just, caches in release

`medium · confirmed · +15 lines · S`  
Where: `.github/workflows/release.yml:23-25`, `.github/workflows/release.yml:65-71`, `.github/workflows/release.yml:191-229`, `.github/workflows/release.yml:270`, `.github/actions/setup-guest-toolchain/action.yml:89-118`

- Claim: Confirmed as stated. One refinement: DOCKERHUB_TOKEN reaches only build-watchdog-image (release.yml:180-208). The broader exposure is the workflow-wide GITHUB_TOKEN with contents:write and packages:write (release.yml:23-25). Every tag-referenced action in every job can use that token, including taiki-e/install-action@v2 (:69), Swatinem/rust-cache@v2 (:66) and dtolnay/rust-toolchain@stable (:60).
- Fix: Pin every non-GitHub-owned action to a commit SHA with a `# vX.Y` comment. Move `permissions` to the job level: contents:write only on `publish`, packages:write only on `build-watchdog-image`, and `contents: read` at the top of both workflows. Pin tool versions (`tool: cross@<ver>,just@<ver>`) and record them in toolchain-pins.env. Drop rust-cache/gha cache from release.yml, since a release build should not restore a cache written by other refs. Add .github/dependabot.yml for `github-actions` so the SHA pins stay current.
- Decision: Whether release builds may keep caches (speed) or must build cold (provenance).

### L2. workflow_dispatch release builds the dispatched branch but labels/tags it with inputs.tag

`medium · confirmed · +6 lines · S · BC`  
Where: `.github/workflows/release.yml:11-21`, `.github/workflows/release.yml:41-42`, `.github/workflows/release.yml:127-128`, `.github/workflows/release.yml:174-175`, `.github/workflows/release.yml:269-273`

- Claim: Confirmed. It matters more than the reviewer says: generate-release-manifest.sh:85-91 writes an explicit `alignment.rule`, 'All artifacts sharing release_tag were built from git_commit'. A dispatch from main naming a new tag breaks that rule, because softprops creates the tag at the default-branch HEAD while git_commit is GITHUB_SHA. A dispatch naming an existing tag is also wrong: it uploads assets built from the dispatched branch into that tag's release.
- Fix: Pass `ref: ${{ inputs.tag || github.ref }}` to every checkout in release.yml, and fail early if `inputs.tag` does not resolve to an existing tag. Alternatively, drop the free-form `tag` input and let dispatch only re-run an existing tag.
- Decision: Should dispatch be allowed to create a new tag at all, or only rebuild an existing one?
- Independent trace: confirmed. Code reading confirms the claim. There are four checkouts and none passes `ref:`.

### L3. CI runs every PR push twice and never cancels superseded runs

`low · partially · +5 lines · S`  
Where: `.github/workflows/ci.yml:3-5`, `.github/workflows/ci.yml:102-105`, `.github/workflows/ci.yml:131-134`, `.github/workflows/ci.yml:182-186`

- Claim: The double run and the missing concurrency group are real: ci.yml:3-5 has no filters and the file has no `concurrency:` key. Two parts are overstated. First, watchdog-docker's `needs: rust` (ci.yml:185) is not 'for no reason'. It is the same fail-fast gate canonical-guest (:104) and rollups-e2e (:133) use, trading latency for wasted minutes on red pushes. Second, `push: branches: [main]` also stops CI on v* tag pushes (today the unfiltered push trigger runs it). release.yml has no CI gate, so a tag commit that never reached main or a PR would then ship with no CI run at all. Treat this as a cost and latency cleanup, not a defect.
- Fix: Use `push: branches: [main]` plus `pull_request:`, and add `concurrency: { group: ${{ github.workflow }}-${{ github.ref }}, cancel-in-progress: true }`. Drop `needs: rust` from watchdog-docker. This roughly halves runner minutes per PR iteration, and the safety net is unchanged because the PR run covers the same tree.
- Decision: The double run has incidentally surfaced flakes (register:150-152). Confirm that losing that is acceptable; a nightly re-run is the explicit alternative.

### L4. Four compiled duplicate stacks are caused by our own version picks, plus one unused direct dep

`low · partially · -2 lines · S · public-rust-api`  
Where: `Cargo.toml:62`, `Cargo.toml:71`, `sdk/guest/testsi/Cargo.toml:23`, `sequencer/Cargo.toml:19`, `sequencer/Cargo.toml:28`

- Claim: The four duplicate stacks exist as described, but none of reqwest 0.12, tokio-tungstenite 0.28 or thiserror 1 is in the shipped wallet-sequencer graph. sequencer pulls reqwest and tokio-tungstenite only as dev-dependencies (sequencer/Cargo.toml:45-46). The other users are the SDK, harness and benchmarks, and thiserror 1 comes only from testsi (sdk/guest/testsi/Cargo.toml:23). That makes this test and CI build time only. alloy-network-primitives is confirmed unused: sequencer/Cargo.toml:28, with zero `alloy_network_primitives` uses in any .rs file. The tokio-tungstenite part is superseded-by-direction. axum's `ws` feature (sequencer/Cargo.toml:14) serves only `/ws/subscribe` (egress/api/mod.rs:36), and the SDK's tungstenite is that endpoint's client (sdk/rust-client/src/lib.rs:25). Both are the subscribe path the new sync replaces, so freeze them rather than dedupe. Keep the reqwest, thiserror, toml and unused-dep parts. The reqwest bump changes the SDK's public API (`pub response: reqwest::Response`, sdk/rust-client/src/lib.rs:33), so coordinate it with the open register entry 'Support HTTPS and WSS in the Rust SDK', which needs a TLS feature choice anyway.
- Fix: Bump `reqwest = { version = "0.13", default-features = false }` and `tokio-tungstenite = "0.29"` in [workspace.dependencies]. Set testsi to `thiserror = { workspace = true }`. Delete `alloy-network-primitives` from sequencer/Cargo.toml:28. Optionally replace `alloy-transport` with `alloy::transports::layers::RetryBackoffLayer` and bump toml to the current major. Check the result with `cargo tree -d -e normal,build --workspace`.
- Decision: sequencer-rust-client is an SDK. If its public API exposes tungstenite/reqwest types, the bump is a semver-visible change. The toml bump changes how checkpoint info.toml is rendered (dump_info.rs:109,159); confirm no digest covers those bytes before bumping.

### L5. Pin plumbing (~150 lines) mostly checks the pins file against itself and misses the real drift points

`medium · partially · -90 lines · S`  
Where: `scripts/verify-toolchain-pins.sh:43-74`, `scripts/load-toolchain-pins.sh:1-49`, `rust-toolchain.toml:2`, `toolchain-pins.env:5`, `toolchain-pins.env:21-22`

- Claim: The core is confirmed. verify-toolchain-pins.sh:47-55 re-reads toolchain-pins.env and compares it with values sourced from the same file. Its only justfile check (:57) greps for `pins := "../../toolchain-pins.env"`, and that variable is dead: examples/canonical-app/justfile:3 defines it, but lines 4-6 hard-code the path. The real drift points go unchecked. Foundry 1.5.1 appears at ci.yml:57, action.yml:30 and justfile:16. The contracts version appears at Cargo.toml:49 and justfile:15, plus a third copy the reviewer missed: tests/harness/src/paths.rs:6-7 hard-codes both 3.0.0-alpha.10 and anvil-1.5.1. Guest-tools 0.18.0 appears at canonical Dockerfile:7 and watchdog/test-guest/Dockerfile:8, and must match libcmt-sys. Two corrections to the proposal. RUST_TOOLCHAIN cannot simply be deleted, because generate-release-manifest.sh:79 reads os.environ["RUST_TOOLCHAIN"] into RELEASE.json; the script must parse rust-toolchain.toml instead. The cited toolchain-pins.env:21-22 is the lua-cURL comment and is irrelevant. A realistic net delta is about -60 to -70 lines.
- Fix: Make rust-toolchain.toml the only Rust pin: delete RUST_TOOLCHAIN, the `rust-toolchain` action input and the dtolnay steps, and use `rustup toolchain install` (adding `targets` where needed). Add FOUNDRY_VERSION, ROLLUPS_CONTRACTS_VERSION and MACHINE_GUEST_TOOLS_VERSION to toolchain-pins.env, and have ci.yml, the justfile and the Dockerfile ARG consume them. Replace verify lines 43-74 with three real cross-file greps (Cargo.toml `=${ROLLUPS_CONTRACTS_VERSION}`, justfile dump name, libcmt-sys description/README). Inline the loader as `grep -E '^[A-Z_]+=' toolchain-pins.env >> "$GITHUB_ENV"`.

### L6. Consensus guest build lacks --locked, uses an unpinned cross binary, and does not remap paths

`low · partially · +2 lines · S`  
Where: `examples/canonical-app/justfile:43`, `examples/canonical-app/justfile:49`, `examples/canonical-app/Cross.toml:3`, `.github/actions/setup-guest-toolchain/action.yml:115-118`, `.github/workflows/release.yml:147-151`

- Claim: The facts are right. The cross builds have no --locked (canonical justfile:43,49), and neither does `cargo run -p canonical-test` (:107). The cross binary is unpinned (action.yml:115-118). There is no .cargo/config.toml or RUSTFLAGS remap anywhere. The impact is smaller than claimed. In CI, canonical-guest and rollups-e2e `needs: rust`, and rust builds with --locked (ci.yml:60), so a stale lock fails first. A release tag on a commit that already passed CI cannot re-resolve. The checkout-path concern is probably moot: cargo passes workspace-relative paths for workspace members, and no path dependencies sit outside the workspace. Adding --locked is still a free one-line fix. Leave the remap concern pending the reviewer's two-path experiment.
- Fix: Add `--locked` to both cross builds (and to `cargo run -p canonical-test`). Pin the cross version (covered by L1). Try a cheap experiment first: run `just canonical print-build-hashes` from two different checkout paths. Add `RUSTFLAGS=--remap-path-prefix=$PWD=/src` only if the hashes differ.
- Decision: Whether independent rebuilds of the template hash from any checkout path are a goal. The remap part is low-confidence until the two-path experiment runs.

### L7. The devnet guest image is built twice per CI run (canonical-guest and rollups-e2e jobs)

`low · confirmed · -25 lines · S`  
Where: `.github/workflows/ci.yml:102-129`, `.github/workflows/ci.yml:131-171`, `examples/canonical-app/justfile:82`, `examples/canonical-app/justfile:106-107`, `justfile:62-71`

- Claim: Confirmed. canonical-guest runs test-guest, which depends on build-machine-image (canonical justfile:106,82). rollups-e2e builds the same devnet image through ensure-machine-image (root justfile:62,67-68), each job after a full setup-guest-toolchain. The e2e path runs lua-deps three times (ci.yml:163, justfile:63, justfile:84) and `apt-get update` a second time (ci.yml:157 after action.yml:46). The ~-25 delta is plausible because it mostly removes ci.yml:102-129.
- Fix: Fold the canonical-guest job into rollups-e2e by running `just canonical-test-guest` first. That builds the image once, and ensure-machine-image then reuses it. Drop the redundant lua-deps invocation and the second `apt-get update` (ci.yml:155-163). The trade-off is one less parallel job and roughly 10 minutes more wall-clock on the e2e path.
- Decision: Choose between parallel wall-clock and the runner minutes saved by building the guest image once.

### L8. canonical-app justfile copy-pastes devnet and sepolia recipes three times

`low · confirmed · -35 lines · S · config`  
Where: `examples/canonical-app/justfile:39-104`, `justfile:89-93`, `.github/workflows/release.yml:147-151`

- Claim: Confirmed. The pairs at examples/canonical-app/justfile:41-51, 55-71 and 82-104 differ only in the bin name or the --store path. dapp_binary, rootfs_tar and rootfs_ext2 (:17-20) are shared across chains. Today this is hygiene, not a bug: each machine-image recipe rebuilds its own rootfs before storing it, so the sequential builds in release.yml:147-151 and justfile:62 come out correct. A realistic net is about -20 to -25 lines once the aliases are added back.
- Fix: Use one `build-machine-image chain="devnet"` recipe with per-chain intermediates under out/<chain>/. Keep the existing output dir names and thin aliases so the harness, watchdog and release consumers do not change.

### L9. Release builds wallet-sequencer through cross even for the native x86_64 target and installs unused apt packages

`low · confirmed · -10 lines · S · BC`  
Where: `.github/workflows/release.yml:28-76`, `.github/workflows/release.yml:47-57`

- Claim: Confirmed. release.yml:76 builds both matrix targets with cross. There is no root Cross.toml, so the default tag-based images are used. lua5.4, liblua5.4-dev, libslirp-dev and libssl-dev are installed (:47-57), but Cargo.lock has no openssl, native-tls or hyper-tls. Cross's older-glibc images are a deliberate portability benefit, which the needs_decision already captures.
- Fix: Either build natively on `ubuntu-24.04` and `ubuntu-24.04-arm` runners, which drops cross and docker from this job, or pin cross and add a root Cross.toml with image digests. Drop the unused apt packages either way.
- Decision: Cross images give an old-glibc, portable binary, while native 24.04 runners raise the glibc floor to 2.39. Pick the supported glibc floor, or move to a static musl target.

### L10. Lint gaps: the production bash launcher and scripts are not shellchecked, one Lua fixture is unlinted, there is no workspace lint table, and `just ci` has drifted from CI

`low · confirmed · +12 lines · S`  
Where: `.github/workflows/ci.yml:65-69`, `watchdog/sequencer-watchdog:1-5`, `scripts/watchdog-lua-deps.sh:84`, `scripts/verify-toolchain-pins.sh:13`, `Cargo.toml:1-73`

- Claim: Confirmed, with one count corrected: there are 8 `allow` attributes, not 6. The two missed are libcmt-sys/src/lib.rs:1-3 (three bindgen allows) and sequencer/src/commands/test_support.rs:29; all remain justified. Nothing runs shellcheck, even though five `shellcheck disable` directives exist (scripts/*.sh) and watchdog/sequencer-watchdog is 67 lines. luacheck at ci.yml:69 misses watchdog/tests/fixtures/input_added_evm_advance.lua. No manifest has a `[lints]` table. `just ci` (justfile:120-124) omits clippy, luacheck, the Lua tests and the C smoke.
- Fix: Add a CI step `shellcheck scripts/*.sh watchdog/sequencer-watchdog` and lint `tests/**/*.lua`. Add `[workspace.lints.clippy] undocumented_unsafe_blocks = "warn"`, opted in first by bindings/c-app-engine (the seam reader). Either delete `just ci` or make CI invoke it so there is one list.
- Decision: Whether to require SAFETY comments on the FFI seam crates (adds about 40 short comments).

### L11. No advisory or source gate on the crate graph

`low · confirmed · +35 lines · S`  
Where: `Cargo.toml:41-73`, `sdk/guest/testsi/Cargo.toml:19`, `.github/workflows/ci.yml:8-81`

- Claim: Confirmed. There is no deny.toml or audit.toml, and no advisory or source step in ci.yml. The only git source is the dave rev (sdk/guest/testsi/Cargo.toml:19). A non-blocking job plus a schedule is proportionate.
- Fix: Add a small deny.toml covering `advisories` and `sources` (allow-git = the dave repo only), with `bans` left at warn. Run `cargo deny check advisories sources` as a separate CI job that does not block PRs, plus a weekly schedule. The invariant it protects is that no known-vulnerable crate ships in the sequencer or watchdog release.
- Decision: Blocking or advisory-only on PRs.

### L12. sequencer-core build.rs generates 15 constants; a checked-in table plus a recompute test is simpler and visible in review

`low · confirmed · -50 lines · S`  
Where: `sequencer-core/build.rs:1-119`, `sequencer-core/Cargo.toml:25-26`, `sequencer-core/src/fee.rs:20-24`

- Claim: Confirmed. build.rs:66-118 generates TABLE and MAX_EXPONENT into OUT_DIR, fee.rs:21-23 include!s them, and ruint is the only build-dependency (sequencer-core/Cargo.toml:25-26). A checked-in literal plus a recompute test gives the same guarantee. It also serves fee.rs:15-16 ('Independent implementations must reproduce the conversion bit-for-bit') better, because the values become reviewable. The fee.rs doc references to build.rs (:14,:44) must be updated with it.
- Fix: Check in the generated `TABLE`/`MAX_EXPONENT` as a literal const in fee.rs. Move the repeated-squaring algorithm into a #[test] that recomputes the table with alloy U256 and asserts equality. That gives the same guarantee, enforced in CI, and the table becomes reviewable.
- Decision: Whether the maintainer prefers derived-at-build constants on principle over a test-pinned literal.

### L13. No profile tuning: full debuginfo for roughly 500 deps on every dev/test build and in the CI cache

`low · confirmed · +4 lines · S`  
Where: `Cargo.toml:1-73`

- Claim: Confirmed: no manifest has a `[profile.*]` section. Add one caution: restrict the change to `[profile.dev]`. The guest is built with `cross build --release` (canonical justfile:43,49), so any `[profile.release]` edit changes the template hash and is consensus-relevant.
- Fix: Add `[profile.dev] debug = "line-tables-only"` (backtraces keep file:line) and optionally `[profile.dev.package."*"] debug = false`. Measure before and after with `cargo build --timings` and `du -sh target`.
- Decision: Accept losing full variable-level debuginfo in dependencies.

### L14. Manifest hygiene: wrong inherited readme path, unhoisted shared deps, guest crates outside workspace.package

`nit · unverified-nit · +0 lines · S`  
Where: `Cargo.toml:34`, `sequencer/Cargo.toml:13`, `sequencer/Cargo.toml:19-20`, `tests/harness/Cargo.toml`, `tests/e2e/Cargo.toml`

- Claim: Inherited `readme = "../README.md"` resolves relative to the workspace root, so it points outside the repo. Shared deps are declared per member: tar 0.4 ×3, toml 0.8 ×2, libtest-mimic 0.6/0.6.1, and the `sequencer-core` path in 8 manifests. rollups-types uses `alloy-* = "1"` instead of the workspace entries. The five guest crates hard-code version, edition 2021 and license instead of inheriting. Only the guest crates set publish=false.
- Fix: Set `readme = "README.md"` and `publish = false` in [workspace.package]. Hoist tar, toml, libtest-mimic and the internal path crates into [workspace.dependencies]. Point rollups-types at workspace alloy. Migrate the guest crates to edition 2024 and `*.workspace = true` in a separate refactor patch (bindgen output may need `unsafe extern`).

### L15. Comments that restate code or describe things that do not exist (representative)

`nit · unverified-nit · -12 lines · S · docs-only`  
Where: `sequencer/Cargo.toml:50-52`, `sequencer-core/build.rs:24-25`, `sequencer-core/build.rs:73`, `sequencer-core/build.rs:82`, `sequencer-core/build.rs:84-86`

- Claim: Examples: (1) sequencer/Cargo.toml:50-52 spends three lines explaining what [dev-dependencies] means; "# TcpProxy for provider-disconnect tests" would do. (2) build.rs repeats 'exact integer, no floating point' three times (:11, :86, :100) and narrates steps (`// Build the table by repeated squaring.`, `// Generate Rust source.`). (3) toolchain-pins.env:21-22 documents a pin that does not exist, duplicating watchdog-lua-deps.sh:6-9.
- Fix: Trim to the non-obvious: keep the alloy 2.x/1.x constraint note (Cargo.toml:51-53) and the Cross.toml digest rationale; cut the rest. This is moot for build.rs if L12 lands.

### L16. Checksum-failure messages hide the actual hash (representative)

`nit · unverified-nit · +2 lines · S`  
Where: `justfile:77`, `examples/canonical-app/justfile:28`, `examples/canonical-app/justfile:36`

- Claim: When a pinned download is replaced upstream, the operator sees 'SHA256 mismatch ... delete it and re-run' on stdout, with shasum's FAILED line discarded by `>/dev/null`. There is no expected-vs-actual pair to tell a corrupted download from a re-published asset. The kernel check (canonical justfile:28) likewise discards shasum's stdout and prints nothing that names the pinned release.
- Fix: On failure, print `expected=<pin> actual=$(shasum -a 256 file)` and the URL to stderr.

### LS1. The guest kernel, a consensus-relevant input, is not hash-pinned in-tree; its checksum is downloaded next to it

`medium · skeptic-added · -3 lines · S · BC · config`  
Where: `examples/canonical-app/justfile:13-16`, `examples/canonical-app/justfile:25-37`, `toolchain-pins.env:17-19`, `watchdog/test-guest/justfile:41`, `justfile:21-24`

- Claim: download-deps fetches both the kernel and its .sha512 from the same machine-linux-image release (justfile:31-32) and checks one against the other. That catches corruption in transit, but not a replaced release asset. Every other download is pinned in-tree: the CM and xgenext2fs .debs (toolchain-pins.env:8-15), guest-tools (canonical Dockerfile:8) and the anvil dump (root justfile:24, whose comment states the 'replaced/corrupted release asset' TOFU model). The kernel goes into the machine image's RAM (canonical justfile:87), so it determines the template hash. The area verdict's 'every downloaded artifact is SHA-pinned' is therefore false for the one input that matters most.
- Fix: Add CARTESI_LINUX_KERNEL_SHA512 (or SHA256) to toolchain-pins.env next to CARTESI_LINUX_KERNEL_FILENAME. Verify the downloaded kernel against the pin in download-deps and in watchdog/test-guest, and drop the second wget and the sed rewrite. This protects the invariant that the template hash is a function of in-tree pins and source only, under the same TOFU model the repo already uses.

### LS2. Tag-push releases are never marked prerelease, so every alpha is published as a full release

`low · skeptic-added · +0 lines · S · BC`  
Where: `.github/workflows/release.yml:273`, `.github/workflows/release.yml:17-21`

- Claim: `prerelease: ${{ github.event_name == 'workflow_dispatch' && inputs.prerelease }}` is false on the normal path, a v* tag push. Every tag so far is v0.1.0-alpha.1..10 (git tag -l), so each tag push publishes a non-prerelease, which GitHub's default make_latest then marks as Latest. Only manual dispatches honour the prerelease flag.
- Fix: `prerelease: ${{ github.event_name == 'workflow_dispatch' && inputs.prerelease || github.event_name == 'push' && contains(github.ref_name, '-') }}`, which marks SemVer pre-release tags automatically.
- Decision: Whether alpha tags should be marked prerelease (likely yes).

Refuted:

- L17. Snapshot-egress-only deps: freeze, don't polish. The reviewer tied sha2 and tokio-util to download-then-subscribe without reading which routes use them.

Keep (load-bearing):

- toolchain-pins.env plus SHA verification of every downloaded input: the CM .deb (install-cartesi-machine/action.yml:38-39), xgenext2fs (setup-guest-toolchain/action.yml:77-79), kernel sha512 (canonical justfile:27-37), guest tools sha512 (canonical Dockerfile:8-13) and the TOFU anvil dump hash (justfile:21-24). These are what make the canonical image and the e2e chain state trustworthy.
- examples/canonical-app/Cross.toml:3 pins the guest cross image by digest; the consensus binary's toolchain must be bit-stable.
- SOURCE_DATE_EPOCH, `xgenext2fs -f` (zeroed timestamps), `--assert-version` and `--assert-rolling-template --final-hash` in the canonical justfile (:43, :62, :84-92). These make the template hash reproducible and catch a wrong emulator early.
- `--locked` on every CI and release cargo call (ci.yml:60,66,78; release.yml:76) prevents silent dependency drift in tested and shipped artifacts.
- default-members = [sequencer, wallet-sequencer] (Cargo.toml:25) keeps plain `cargo build` free of the CM/libclang/libslirp requirement that canonical-test and testsi impose.
- scripts/ci-c-application-smoke.sh builds an out-of-workspace consumer against the C seam, which catches workspace-only assumptions in c-app-sequencer that in-workspace tests cannot.
- scripts/ci-watchdog-docker-smoke.sh exercises the real production entrypoint, the flock contention and the uninitialized-state path inside the shipped image, not just that the image builds.
- c-app-engine generates its FFI bindings with bindgen from the header at build time (bindings/c-app-engine/Cargo.toml build-deps comment): the header is the ABI source of truth, and checking in generated bindings would let them drift.
- libcmt-sys builds a host mock (sdk/guest/README.md), which is why clippy and tests cover guest crates on the host with no RISC-V lint job.
- Workspace alloy feature lists are member-specific and `node-bindings` is dev-only (sequencer/Cargo.toml:41-43). This keeps the Anvil-spawning surface out of production builds.
- E2E failure-log upload (ci.yml:173-180) is the instrumentation the open flake register entry depends on.
- Release SHA256SUMS + release-manifest.json (release.yml:251-267) give operators one artifact that ties the sequencer, the watchdog image and the canonical images to one commit and one set of pins.

## M. Watchdog (Lua) and guest SDK provenance

Verdict: The watchdog is a one-shot Lua CLI of about 2,240 production lines plus a 67-line bash wrapper. It replays InputBox inputs on the canonical Cartesi Machine in-process, proves the L1 view complete (contiguous indices plus getNumberOfInputs pinned at B), compares a flat SHA-256 against the sequencer's accepted checkpoint, latches a divergence before collecting evidence, and offers `replay` for recovery. The size fits the essential complexity. No module passes 320 lines, each concept has one home, there is no daemon and no in-tick retry, there are only three failure classes, and the README's "Why it is built this way" names the invariant behind each hardening mechanism. The v2 rewrite (c0bebe8 and later) overtook both backlog notes: AI3 is fixed, and AI2 is down to a small residual in idempotent init. The real problems are small and local: (1) the credential-bearing RPC URL leaks into stderr, last_tick.json and `status` output (http.lua:73), the same shape as an open register entry; (2) config.json mixes deployment identity with endpoint and provider tuning that operators rotate; (3) init's no-config branch deletes an agreed head instead of failing loud; (4) module headers restate the README. Nothing here is superseded by the recovery and client-sync direction. `replay` is the canonical-side input the future shared recovery tool will consume, so keep it, and do not grow application-specific export inside the watchdog. sdk/guest is clearly vendored: libcmt is pinned by tag, commit and tarball sha256 and has update steps, and the Rust crates are an acknowledged in-tree fork. The one gap is that the guest-tools/libcmt version pairing is held only by prose in two files.


### M1. L1 RPC URL (often carrying an API key) is embedded in failure messages that reach stderr, last_tick.json and `status`

`medium · confirmed · +0 lines · S · BC`  
Where: `watchdog/http.lua:73`, `watchdog/jsonrpc.lua:36-37`, `watchdog/l1.lua:81-84`, `watchdog/main.lua:271-274`, `watchdog/main.lua:176-182`

- Claim: On any transport failure (DNS, refused connection, timeout), http.lua:73 formats the full request URL into the error. For the L1 RPC that is the provider URL, and Infura or Alchemy style URLs carry the API key in the path. jsonrpc.lua:36-37 passes the error through unchanged, and l1.lua:83 and l1.lua:205 wrap it as a transient error. main.lua:272-274 then logs it to stderr, main.lua:176-182 persists it in last_tick.json, and main.lua:146 prints it from `status`. This contradicts docs/watchdog/README.md:143-144 ('The L1 RPC endpoint is never persisted'). status.prom is not affected, because metrics.render emits no message (metrics.lua:44-73). The libcurl error text itself names at most the host. HTTP-status failures (jsonrpc.lua:45-48) do not include the URL.
- Fix: Drop the URL from http.lua's message and keep only the method. Callers already add context ('L1 RPC:', 'sequencer <route>:'). If a host is wanted, log scheme://host only. Fix it in the same patch as the Rust register entry.

### M2. config.json persists rotatable endpoint/provider tuning (sequencer_url, long_block_range_error_codes) next to deployment identity

`low · partially · -12 lines · S · BC · config`  
Where: `watchdog/config.lua:4-7`, `watchdog/config.lua:99`, `watchdog/config.lua:107`, `watchdog/config.lua:123`, `watchdog/config.lua:129`

- Claim: Two facts hold. init requires CARTESI_WATCHDOG_SEQUENCER_URL (config.lua:99) although bootstrap.run never uses a sequencer (bootstrap.lua:20-62, main.lua:125). The range-error codes are frozen at init (config.lua:107, :129, :148-161), so switching to a provider with a different code means wiping the state directory and re-initializing, per README.md:162-163. But 'two sources of truth' overstates it. The URL is a documented persisted default with a tick-time override (README.md:150, config.lua:154), and the module header says so (config.lua:4-7). The failure mode is also loud and bounded: a missing code only makes wide getLogs ranges fail as transient (l1.lua:196-205), while small ranges keep working, and the default list covers the common providers. Proposed delta is closer to -5 than -12, because the tick needs a required-URL check in its place.
- Fix: Read both from the env at run time (codes keep their default) and stop persisting them. config.load ignores the old fields, so no version bump is needed. config.json then holds pure identity: version, chain_id, app_address, input_box_address, state_source, bootstrap_block. The AI2 element validation goes away with the field.
- Decision: Should the sequencer URL become a required tick-time env var (like the RPC URL), and should provider error codes be tunable without re-init?

### M3. Idempotent init checks only identity, not the schema of an existing config.json (AI2 residual)

`nit · partially · +2 lines · S · BC`  
Where: `watchdog/bootstrap.lua:23-35`, `watchdog/config.lua:133-163`

- Claim: The idempotent init branch compares only chain_id, app_address and state_source (bootstrap.lua:23-35, config.lua:134-138) and never runs config.load. The gap is reachable only through a config.json that was hand-edited but is still valid JSON. A torn or corrupt file already fails at bootstrap.lua:23, because read_json raises an operator error (store.lua:119-121). Every value init writes is validated before it is written (config.lua:94-130). A v1 directory gets 'initialized for another deployment ... wipe it to re-initialize' (bootstrap.lua:28-29) rather than the version message (config.lua:145), but both prescribe the same remedy (wipe and re-init). Calling config.load in that branch is still a harmless one-line tightening.
- Fix: In the idempotent branch, call `config.load(store.dir, existing, {})` before comparing identity (one line). Init then fails with the same diagnosis tick would give.

### M4. init silently deletes an agreed head when config.json alone is missing, which is an impossible state that should fail loud

`low · confirmed · +6 lines · S · BC`  
Where: `watchdog/bootstrap.lua:38-40`, `watchdog/bootstrap.lua:54-57`, `watchdog/main.lua:112-118`, `docs/recovery/cockroach.md:85-97`

- Claim: With no config.json, bootstrap.lua:38-40 deletes checkpoints/ wholesale on the assumption that any checkpoint came from an interrupted init. It does so before validating anything: the removal at line 40 runs before the chain-id, InputBox, source and trust checks at lines 43-52. An interrupted init can leave only one checkpoint, at its bootstrap block (lines 54-57 publish before config.json is written), and an agreed head always has block > bootstrap_block (tick.lua:72, :106). So a missing config.json beside an agreed head is an impossible state that gets 'handled' by deletion. The code's own diagnostics lead into it. A missing config.json yields 'not initialized; run init' (main.lua:115; operator-deployment.md:395 attributes that message only to an empty directory). operator-deployment.md:250-253 endorses running init before every tick. operator-deployment.md:177 says config.json 'needs no backup', which contradicts cockroach.md:85. Init then exits 0 and silently forces a full replay from the bootstrap snapshot. A CM_SNAPSHOT_DIR pointing inside checkpoints/ is deleted before it is loaded. Safety holds (the re-bootstrap is from a trusted snapshot), and backups kept outside the state directory survive (operator-deployment.md:183-184), so the severity stays low. Better proposal: move the removal after validation; remove only a single checkpoint whose block equals cfg.bootstrap_block; raise an operator error ('checkpoints without config.json; restore config.json or wipe checkpoints/') otherwise, including when bootstrap_dir lies under checkpoints/; and fix the operator-deployment.md:177 wording.
- Fix: In the no-config branch, remove only a checkpoint whose block equals cfg.bootstrap_block. Raise an operator error if any other checkpoint exists. Invariant: an agreed head is deleted only by a newer agreement.

### M5. Error messages missing context or unbounded: rmdir without a path, full HTTP error bodies

`low · confirmed · +2 lines · S · BC`  
Where: `watchdog/store.lua:66`, `watchdog/tick.lua:108-113`, `watchdog/sequencer.lua:24-26`, `watchdog/jsonrpc.lua:48`

- Claim: remove_tree's `assert(lfs.rmdir(path))` (store.lua:66) raises only strerror. The vendored lfs remove_dir calls pushresult with a NULL info (luafilesystem/src/lfs.c:617-621, 221-229), so a failed prune (tick.lua:109-113) logs 'internal failure: Directory not empty' with no path. os.remove on line 68 does name the file. Separately, require_ok puts the whole non-2xx GET body into the error (sequencer.lua:25). jsonrpc truncates to 200 bytes (jsonrpc.lua:48), and http:download already caps its error body at 512 bytes (http.lua:113-116). So only the inclusion_block and digest GETs are unbounded.
- Fix: Raise `cannot remove <path>: <err>` in remove_tree, and truncate the sequencer body to the same 200 bytes as jsonrpc.

### M6. Module headers and the test-guest README restate docs/watchdog/README.md and the e2e oracle

`low · partially · -80 lines · S · docs-only`  
Where: `watchdog/store.lua:4-21`, `watchdog/tick.lua:4-21`, `watchdog/incident.lua:4-14`, `watchdog/incident.lua:104-116`, `watchdog/sequencer-watchdog:16-28`

- Claim: The module headers in store.lua:4-21, tick.lua:4-21 and incident.lua:4-14 do restate README sections (README.md:24-48, 165-193, 195-224), and one copy has already drifted. store.lua:18-21 says config.json is 'written once by init, which is re-run after a crash', but a torn config.json makes init itself fail (bootstrap.lua:23, store.lua:119-121), and README.md:184-186 and operator-deployment.md:394 say to wipe first. The wrapper's `case`-vs-`[[` aside (sequencer-watchdog:27-28) is self-evident. Two parts of the proposal are wrong. (1) incident.lua:104-116 is collect()'s parameter contract (the shape of `evidence`), not a README copy, so keep it. (2) The test-guest recipe (test-guest/README.md:60-89) cannot be replaced by e2e.lua. e2e's oracle compares final root hashes only (e2e.lua:84-137), not CLI exit codes 1/7/101 or the inspect-after-exception behavior documented at lines 99-117. The outcome table (lines 93-98) is keyed by the recipe's run names. Also keep the wrapper sentence on why the paths are appended, since it guards against a real regression. A realistic delta is about -40 lines, not -80.
- Fix: Cut each module header to one line plus the one non-obvious invariant and a README anchor. Cut the wrapper comment to one line. In test-guest/README keep the outcome table and the emulator quirks (lines 99-117) and replace the bash recipe with a pointer to e2e.lua.

### M7. `status` fails entirely on a torn last_tick.json or config.json, right after the host crash when operators most need it

`nit · partially · +4 lines · S · BC`  
Where: `watchdog/main.lua:130-148`, `watchdog/store.lua:113-124`, `watchdog/incident.lua:32-45`

- Claim: `status` reads config.json and last_tick.json without pcall (main.lua:133, :146), so invalid JSON fails the whole command (store.lua:119-121), while the incident reads degrade gracefully (incident.lua:32-35, 41-44). The window is small. A torn last_tick.json lasts at most one tick period, because tick never reads that file and record() rewrites it on every run (main.lua:176-182). A torn config.json also fails tick and init, and the documented remedy is to wipe the state directory (README.md:184-186). During the window, the latch stays visible through the tick's exit 2 and cartesi_watchdog_divergence_info in status.prom (metrics.lua:60-64). Wrapping the two reads is fine, but this is robustness, not 'tests'.
- Fix: Wrap both reads in `status` the way incident.evidence does, reporting `{unreadable = true}` for that field so the rest of the summary still prints.

### M8. Provenance gaps: json.lua has no pinned commit, and the guest-tools/libcmt pairing is held by prose in two files

`low · partially · +8 lines · S`  
Where: `watchdog/third_party/json.lua:2-4`, `watchdog/third_party/lua-curl/UPSTREAM`, `watchdog/third_party/luafilesystem/UPSTREAM`, `sdk/guest/README.md:19-28`, `examples/canonical-app/Dockerfile:5-8`

- Claim: json.lua names only '@ 0.1.2 (commit on master)' plus a local change (third_party/json.lua:3-5), while lua-curl and lfs have UPSTREAM files with a commit or tag (and lfs also a sha256). The guest-tools pin is wider than 'prose in two files'. The version and its sha512 are duplicated in two Dockerfiles (examples/canonical-app/Dockerfile:7-8 and watchdog/test-guest/Dockerfile:8-9), and the libcmt version is repeated in sdk/guest/README.md:17-28, libcmt-sys/Cargo.toml:6 and libcmt-sys/build.rs:5. toolchain-pins.env has no guest-tools entry. scripts/verify-toolchain-pins.sh already checks drift for the other pins (CM version, linux image, kernel), so the proportionate fix reuses it. Add MACHINE_GUEST_TOOLS_VERSION and its sha512 to toolchain-pins.env, then add a few lines to the existing script that require both Dockerfile ARGs to match it (or pass them as --build-arg). This is not new machinery.
- Fix: Add watchdog/third_party/json.UPSTREAM with the upstream commit and a one-line description of the %d change. Move MACHINE_EMULATOR_TOOLS_VERSION (and its sha512) into toolchain-pins.env next to CARTESI_MACHINE_VERSION so one file states the pairing. No verification machinery: updates are rare.

### M9. Dead and duplicated small helpers

`nit · unverified-nit · -20 lines · S`  
Where: `watchdog/abi.lua:92-97`, `watchdog/machine.lua:196-201`, `watchdog/abi.lua:64-68`, `watchdog/json.lua:6-12`, `watchdog/tick.lua:34-37`

- Claim: abi.decode_bytes has no caller. machine.sha256 re-implements abi.hex_from_bytes. The json facade's new() returns a shared module. tick.run re-checks the latch that main already checked (main's check exists because config may be unreadable). The timestamp format is written twice in main. tick injects machine.publish into the evidence data table as a capability.
- Fix: Delete decode_bytes. Reuse a single hex helper. Make watchdog/json.lua `return require("watchdog.third_party.json")` (or require it directly). Keep exactly one latch check, the one in main.lua with tests going through main or asserting a precondition. Pass `machine` to incident.collect instead of storing publish in evidence. Reuse now().

### M10. Two small places that do not fail loud on malformed L1 data

`nit · unverified-nit · +3 lines · S · BC`  
Where: `watchdog/jsonrpc.lua:14-19`, `watchdog/jsonrpc.lua:87`, `watchdog/l1.lua:35-40`, `watchdog/abi.lua:106-111`

- Claim: hex_to_integer uses tonumber(s, 16), which in Lua 5.4 silently wraps for more than 16 hex digits and goes negative at or above 2^63. A malformed safe-head or log position can therefore become a plausible number. decode_evm_advance guesses whether a selector is present from length parity instead of checking for EvmAdvance 0x415bf363.
- Fix: Reject more than 16 hex digits or a negative result in hex_to_integer. Require the EvmAdvance selector and raise otherwise.

### M11. Image-side Lua search-path redundancy

`nit · unverified-nit · -2 lines · S`  
Where: `watchdog/Dockerfile:80-81`, `watchdog/Dockerfile:97-101`, `watchdog/main.lua:13-19`, `watchdog/sequencer-watchdog:22-52`

- Claim: The image copies share/lua/5.4 into both /usr/local/share and /usr/share. It sets CARTESI_WATCHDOG_LUA_DEPS and LUA_CPATH to the same /opt/watchdog/lib, which main.lua then prepends again. The wrapper's path composition serves the documented local-build (host .deb) deployment and is load-bearing. The image duplicates are not.
- Fix: Drop the second COPY (the wrapper already appends /usr/share/lua/5.4) and drop CARTESI_WATCHDOG_LUA_DEPS from the image ENV, or drop /opt/watchdog/lib from LUA_CPATH. Keep the wrapper logic. docker-smoke guards the change.

### MS1. `clear` creates a missing state directory, so a mistyped path during an incident reports 'no divergence is latched'

`low · skeptic-added · +2 lines · S · BC`  
Where: `watchdog/sequencer-watchdog:55-57`, `watchdog/main.lua:96-110`, `watchdog/main.lua:150-153`, `watchdog/incident.lua:187-191`

- Claim: The wrapper runs `mkdir -p` on the state directory for `clear` as well as init and tick, and commands.clear calls open_store(state_dir, true). So a mistyped CARTESI_WATCHDOG_STATE_DIR on `clear` creates an empty directory plus run.lock, and the command fails with 'no divergence is latched'. During an incident this reads as 'already cleared' while the real watchdog keeps exiting 2. It also contradicts main.lua:96-97, which says only commands that write may create the directory so that a mistyped path fails. `clear` only ever acts on an existing incident/ and has nothing to create.
- Fix: Open the store without create in commands.clear, and limit the wrapper's mkdir to init and tick. For clear, fail when the directory is missing before taking the lock. Roughly a 3-line change.

Keep (load-bearing):

- Latch-before-evidence ordering (main.lua:203-286, incident.lua:92-102, tick.lua:45-55): exit 2, the watchdog_event line and status.prom must not wait on multi-GiB evidence collection or be lost to a full disk.
- The `incident/` directory as the latch, with an unreadable record still latching as unreadable_marker (incident.lua:28-37): JSON is not fsynced, so the directory and not the file carries the latch.
- L1 completeness proof (l1.lua:164-233): contiguous indices plus getNumberOfInputs pinned at B turn silent log truncation by a provider into a failed tick instead of a false comparison; the split rule is shared with Rust through the partition vector.
- Deriving the InputBox from Application.getInputBox() at init (l1.lua:92-123): a configured address could name another release's InputBox, whose empty history would look complete.
- Clone-per-input executor with the revert-root assertion (machine.lua:203-248): mirrors the reference CLI's --revert-mode=stored and is tested against the CLI oracle (e2e.lua:89-140).
- Target block fixed from /digest before replay (tick.lua:67-74): a long replay never chases a moving target.
- The bootstrap special case in behind() (tick.lua:57-65): tells a never-agreed head from a real regression.
- Head-by-checkpoint-name with sync_stored+rename publish and no pointer file (store.lua:149-176, machine.lua:110-115): a crash leaves either the old head or the new one.
- check_source plus the trusted_start template-hash check at init (bootstrap.lua:51-52, canonical.lua:18-26): this is what closed AI3.
- Three-class errors module (errors.lua): 44 lines, and it maps cleanly to exit 1 with the class named in logs and last_tick.json.
- `replay` (replay.lua): the canonical-side producer of a checkpoint at a chosen block that cockroach recovery uses today and the future shared recovery tool will consume. Keep it, but keep application-specific export out of it.
- The wrapper's flock and Lua path composition (sequencer-watchdog:29-65): the lock serializes writing commands, and the path logic serves the documented host-.deb local-build deployment.
- Vendored native modules compiled from in-tree sources with UPSTREAM files (third_party/lua-curl, luafilesystem): no build-time download.
- The test guest plus CLI-oracle e2e (watchdog/test-guest, tests/e2e.lua): the only proof that the in-process executor matches the reference host semantics.

## N. Docs corpus and comment verbosity

Verdict: The docs corpus is about 7,350 lines and 50.8k words (README 569, AGENTS.md 464, docs/** 6,314). Production Rust has 4,958 comment lines against 27,396 code lines. The essential content is real: the duality map, the invariant register, scheduler semantics, recovery and its TLA+ models, the watchdog runbook. But the corpus is about 1.5-2x the size that content needs. Three habits cause most of it. First, the same fact is restated in many places: the fee derivation appears in 7 places, the danger-arm order in 4, the count semantics in 5, and the scheduler-mirror co-location note in 3. Second, essays sit where a sentence should: a 60-line rustdoc on check_danger, threat-model table cells over 400 characters, and 37 of 80 module headers breaking the existing 1-4 line rule. Third, finished material stays in the tree: two dated records whose retirement condition is met, and a 546-line TLA+ model of a rejected design. About 700 lines document the egress sync, cross-generation checkpoints and projection replay, which the planned shared rebuild tool replaces; those should be frozen, not polished. The existing rules on review codenames and history narration mostly hold (no codenames found, about 6 narration phrases), so the drift is about repetition and length rather than tone. AGENTS.md costs about 4.9k words in every session and could be about half that size by keeping the duality map, the fail-loud and complexity principles, Always/Ask/Never, the commands and the Reading Routes, and linking out for everything else. On logs, the sampled error and warn calls carry structured context; the one known secret leak (the RPC URL in the startup log) is already an open register entry.


### N1. Freeze the egress-sync / cross-generation / projection-replay docs (~700 lines)

`medium · partially · -650 lines · M · docs-only`  
Where: `docs/protocol/projection-replay.md:1-125`, `docs/plans/2026-07-track3-feed-replay-design.md:1-120`, `docs/review/2026-09-16-track3-validation.md:1-150`, `docs/protocol/application-history.md:70-103`, `docs/protocol/application-history.md:120-178`

- Claim: The cited docs (~700 lines) do describe the shared-state client sync that the direction replaces: projection-replay.md:1-9 (readers rebuild projections from historical L1 inputs), cockroach.md:140-161 (an indexer replays through the new era's C and rebinds its claim), README.md:271-440 (WS claim plus /history and /historical-l1-inputs), and the Track 3 plan. The freeze posture is right. Two parts of the deletion plan are wrong. (a) docs/review/2026-09-16-track3-validation.md also holds the POST /tx ACK latency regression evidence that the ADR's performance posture relies on (2026-08-authority-boundary-adr.md:186-193). The /tx path is not superseded, so move those ACK numbers to a benchmark record before deleting the file. (b) application-history.md:70-103 (checkpoint compatibility after standard recovery) is replaced by the future sync design, not by the cockroach rebuild tool, because automatic recovery still bumps the generation. The freeze note should also fix coordination-tracks.md campaign item 1 ('Merge the implemented egress API... Bart can then integrate'), which is stale now.
- Fix: Do not edit these docs for verbosity now. In one small docs patch, replace Track 3's table row and section in coordination-tracks.md with 3-4 lines recording the new direction and naming these docs as frozen. Leave the contract docs untouched, since AGENTS forbids amendment banners in current contracts. When the shared rebuild tool lands, delete projection-replay.md, the Track 3 plan and the 2026-09-16 record, cut application-history.md to its coordinates table (about 60 lines), and rewrite README.md:271-440 against the new sync. That sequencing saves about 650 lines without double work.
- Decision: Confirm that Track 3 is formally closed and that the 2026-09-16 latency numbers are no longer needed as the ADR's 'performance posture' regression evidence (docs/plans/2026-08-authority-boundary-adr.md:186-193).

### N2. AGENTS.md baseline (464 lines, 4,874 words) restates its owners; a lean baseline is ~230 lines

`medium · partially · -220 lines · M · docs-only`  
Where: `AGENTS.md:145-180`, `AGENTS.md:182-209`, `AGENTS.md:211-233`, `AGENTS.md:235-255`, `AGENTS.md:286-320`

- Claim: AGENTS.md (464 lines, 4,874 words) restates owners in several places. The Application Trait Contract says 'the essentials follow' after naming its owner (AGENTS.md:237), and the module map overlaps README Project Layout. A leaner baseline is reasonable. The proposal would lose facts and links, though. 'Use a dedicated submitter address' and the devnet account-9 fact (AGENTS.md:312-315) exist nowhere else, so they need a new home such as README Running or cockroach.md before Command Configuration is cut. docs/review/register.md:192 (the open OpenAPI-drift entry) links AGENTS.md#http-endpoints for the openapi change-together rule, so turning HTTP Endpoints into a Reading Routes row breaks that anchor and the rule. AGENTS.md:337-340 also endorses an 'intentional baseline' on purpose, so how much to keep is the maintainer's call, not a rule violation.
- Fix: Keep these: Mission plus Requirements (10 lines); Design Principles compressed (12); the duality map with its three implementations and two mechanical facts (25); staleness rule plus the two recovery paths in 8 lines with links; Threat Model brief (8); the fail-loud and comment rules (8); Documentation Practice (12); Shell and Commands (15); Always/Ask/Never (25); Definition of Done (6); Reading Routes (22); a 6-term glossary (chunk, frame, batch, Tip, safe_block, inclusion lane); Type Boundaries (6). Cut the Application Trait Contract to 3 lines plus a link, and Domain Truths to the deposit/rejection/nonce one-liners. Keep the module map here and delete README's copy (see N11). Command Configuration and HTTP Endpoints become a Reading Routes row each. This saves about 2.4k words per session.
- Decision: Is the module map baseline material (keep it in AGENTS) or reachable via README and module headers (cut it)? Recommend AGENTS, since agents need it first.

### N3. Fee derivation / oracle policy is stated in 7 places; batch.rs carries a 33-line fee essay

`low · partially · -45 lines · S · docs-only`  
Where: `sequencer-core/src/batch.rs:7-39`, `README.md:501`, `AGENTS.md:190`, `AGENTS.md:218`, `docs/threat-model/README.md:24`

- Claim: The real duplicate is the 33-line header at sequencer-core/src/batch.rs:7-39. It restates, nearly line for line, the schema comment block at 0001_schema.sql:540-562: the formula, log base, oracle conversion and tenfold slack. The schema also repeats the tenfold slack at :578. README.md:501 and AGENTS.md:218 add clause-level repeats. fee.rs:226 is not a restatement: it tells SUGGESTED_MAX_FEE_SLACK apart from log_slack, a non-obvious note worth keeping. AGENTS.md:190 is already a one-line glossary entry with an owner link. Realistic saving is about 30 lines, which makes this low, not medium. Track 5 (coordination-tracks.md:16) notes that log-space fees may become defunct, which argues for trimming to a pointer rather than polishing.
- Fix: Pick two owners: the schema comment beside batch_policy_derived owns the formula, and the threat-model oracle row owns source trust and outage policy. Example rewrite of the batch.rs header: `// Batch sizing and frame fees are log-space DA charges; the formula and\n// its knobs live with batch_policy_derived in 0001_schema.sql.` Cut README.md:501 to one line plus a link, and drop the log_slack clause from AGENTS.md:218.

### N4. check_danger rustdoc is 60 lines, states the arm order twice, and duplicates the recovery README

`medium · confirmed · -52 lines · S`  
Where: `sequencer/src/storage/recovery.rs:173-232`, `docs/recovery/README.md:130-149`, `docs/invariants.md:152-158`, `docs/invariants.md:459-461`

- Claim: The arm order is documented in the rustdoc as a numbered list (1-6), then as a 'Returns the first variant' chain, then as a five-bullet 'trust hierarchy'. Clock faults are explained twice inside the same comment. The recovery README already owns the order, names check_danger_in, and gives the rationale; I4 and Do-not-simplify repeat it again.
- Fix: Rewrite to: `/// Classify danger in one read transaction. Arms fire in order: divergence,\n/// stale view, closed batch, Tip, clock fault, estimate. The order is the\n/// dispatch policy (I4; docs/recovery/README.md "When safe-head progress stops").\n/// now_ms is injected so storage tests need no clock mocking.`

### N5. Module-header rule (1-4 lines) is broken by 37 of 80 headers, 24 of them over 10 lines (606 lines total)

`medium · confirmed · -250 lines · M`  
Where: `AGENTS.md:328`, `sequencer/src/storage/mod.rs:4-29`, `sequencer/src/commands/run/workers.rs:4-26`, `sequencer/src/storage/lifecycle.rs:4-25`, `sequencer/src/l1/submitter/worker.rs:4-25`

- Claim: Counts reproduced exactly: 80 files with //! headers, 37 over 4 lines, 24 over 10, 606 lines in total. One correction makes the case stronger. storage/mod.rs:6-25 does not list 'all 13 sibling modules': the crate declares 15 (storage/mod.rs:31-45), and the header leaves out `open` and `safe_accepted_batches`. The enumerated list has already drifted. Apart from workers.rs, the convert.rs header (24 lines) also states a real governing posture: contract-bound conversions fail loud, clock and query bounds saturate. Compress it, don't drop it.
- Fix: Run one pass over the 24 headers longer than 10 lines and cut each to its purpose and governing constraint, about 4 lines. Keep the non-obvious build-enforcement note in workers.rs:20-26, which justifies the explicit shape and is cut-resistant; compress it to 3 lines. For storage/mod.rs, keep 'one file per writer role; schema and valid_* views in migrations/0001_schema.sql' and delete the file list. Do this as its own docs-only patch.

### N6. Scheduler-mirror co-location note appears 3 times; 'orthogonal parameter' trivia twice

`low · confirmed · -32 lines · S`  
Where: `sequencer/src/storage/safe_accepted_batches.rs:91-130`, `sequencer-core/src/protocol.rs:344-347`, `sequencer-core/src/protocol.rs:399-422`, `docs/invariants.md:110-114`

- Claim: Three places explain why populate_safe_accepted_batches keeps an inline nonce advance instead of advance_expected_batch_nonce, and one of them narrates a past refactor. Two places explain why batch_submitter is a parameter rather than a field, which is not a contract.
- Fix: Cut the populate_safe_accepted_batches doc to about 8 lines: `/// Mirror scheduler acceptance over new submitter inputs into safe_accepted_batches;\n/// a foreign or byte-different acceptance records canonical_divergence instead (I9, I15).\n/// The only persistent cursor is the latest accepted row: recovery can reuse a nonce\n/// after rejected rows, so those are rescanned until a later acceptance. The expected\n/// advance stays inline because it interleaves with the divergence check (I1).` Delete protocol.rs:344-347 and :420-422; I1 owns the co-location.

### N7. 21-line rustdoc on a #[cfg(test)] helper

`low · partially · -19 lines · S`  
Where: `sequencer/src/storage/ingress.rs:116-138`

- Claim: ensure_open_tip (ingress.rs:116-136, #[cfg(test)] at :137) carries a 21-line rustdoc and should shrink. One sentence must move rather than be deleted. The genesis Tip's leading safe inputs are 'sequenced but not executed', and the lane's catch-up replay executes them, so there is no cold-start drain (:124-126). That fact exists only on this test helper. The production mechanism's doc, open_fresh_tip_in_tx at ingress.rs:468-479, says only 'draining all currently-undrained safe inputs into its first frame', which reads as if they were executed. Move the sentence there. Net saving is about 16 lines.
- Fix: Rewrite to: `/// Test-only: open the genesis Tip on a fresh DB (no-op if a Tip exists).\n/// Production uses ensure_open_tip_for_recovery, which rechecks startup facts.`

### N8. Retire the 2026-09-18 and 2026-09-19 review records: their retirement condition is met

`low · confirmed · -158 lines · S · docs-only`  
Where: `docs/review/2026-09-18-stack-review-validation.md:8`, `docs/review/2026-09-19-review-followup-validation.md:9`, `docs/review/register.md:145-146`

- Claim: Both records say 'Retire this record after (stack) landing when no ongoing review decision uses it'. The stack has landed on main, and the only incoming link is register.md:146, which merely points at flake reproduction context.
- Fix: Delete both files (158 lines). In register.md:145-146 replace the link with 'Reproduced 2026-09-18 under a parallel run; isolated and serial runs passed.'

### N9. docs/recovery/history keeps a 460-line TLA+ model of a rejected design, run by check-all

`low · partially · -520 lines · S · docs-only`  
Where: `docs/recovery/history/README.md:1-77`, `docs/recovery/history/optimistic.tla`, `docs/recovery/history/optimistic.cfg`, `docs/recovery/justfile:12-16`, `docs/recovery/README.md:249-254`

- Claim: The facts are right: optimistic.tla is 460 lines plus a 77-line README, the justfile's check-all includes the roughly 3-minute check-optimistic (docs/recovery/justfile:12-16), and only recovery/README.md:252 links the history. The justification is wrong in two ways. docs/review/README.md:33 governs review notes, not recovery design docs. And the model is not just a 'rejected design': history/README.md:17-19 records a clean 194M-state check of the optimistic alternative after the Silver-only fix, and :62-70 states the downtime tradeoff that would reopen it. That makes it a revisit-trigger asset in the sense of the 'preserve the reason for a deliberate tradeoff together with the assumptions that would change the decision' rule (docs/review/README.md:41-42). The cheaper option that needs no decision is to drop check-optimistic from check-all (one line) and keep the directory. Deleting it stays the maintainer's call.
- Fix: Inline the roughly 25-line counterexample under 'Why the closed-suffix policy is safe' in docs/recovery/README.md. Delete docs/recovery/history/ and the check-optimistic recipe. Record the removal commit in that paragraph so the model can be recovered with git show.
- Decision: Does the maintainer want to keep the bounded optimistic model as runnable evidence? The memory notes a recovery TLA+ design-doc location, so it may be valued.

### N10. README environment-variable list is incomplete versus config.rs and contains history narration

`low · confirmed · -4 lines · S · docs-only`  
Where: `README.md:143-146`, `README.md:166`, `README.md:542`, `AGENTS.md:317-320`, `sequencer/src/commands/config.rs:89-101`

- Claim: README's required/optional variable lists omit the fee-oracle source variables that setup pins, MAX_BATCH_OPEN_SECONDS, FEE_ORACLE_POLL_INTERVAL_MS, and the CHECKPOINT_*/RECOVERY rebuild variables. AGENTS.md:317-320 already says config.rs owns these names. README.md:166 is 'previously/now' narration, and README.md:542 ends in a dangling colon.
- Fix: Keep the two required-variable lines and the example. Replace the optional list with 'All options, defaults and validation: `<binary> setup --help` / `run --help` (config.rs).' Delete line 166 and fix line 542.

### N11. README duplicates AGENTS (module layout, reading list, test prerequisites); snapshots/README is an index-only page

`low · partially · -55 lines · S · docs-only`  
Where: `README.md:503-534`, `README.md:540-550`, `README.md:552-565`, `AGENTS.md:152-180`, `AGENTS.md:375-379`

- Claim: README Project Layout (README.md:503-520) overlaps AGENTS Workspace and module layout (AGENTS.md:152-180), and the two have already drifted: README omits canonical-app, canonical-test and sdk. Further Reading (README.md:552-565) overlaps Reading Routes. Two corrections. docs/snapshots/README.md:8-11, the three-boundary paragraph ('Counts and clocks alone do not establish acceptance'), is not repeated in lifecycle.md; a grep finds it only in snapshots/README.md. Fold it, don't drop it. And there is one inbound link (README.md:560), not 3-4.
- Fix: README keeps a 5-line workspace overview plus 'see AGENTS.md#architecture-map'. Replace Further Reading with a link to AGENTS Reading Routes. Fold docs/snapshots/README.md's two useful sentences into lifecycle.md's intro and delete it, updating 3-4 links.

### N12. Threat-model actor table hides essays in table cells (9 cells, 7.6k characters)

`low · partially · +0 lines · S · docs-only`  
Where: `docs/threat-model/README.md:18-30`

- Claim: Only 4 of the 9 lines over 400 characters are actor-table cells: lines 22, 23, 24 and 28, about 4.2k characters, with the Uniswap row at 1,475. The other five (lines 43, 81, 85, 113, 115) are one-line prose paragraphs or bullets in Self-trust, In-scope, Out-of-scope and L1 block-time coupling. They are long but are not table essays. The proposal to split the oracle, RPC and InputBox policy out of the cells still stands.
- Fix: Limit each cell to one sentence (trust level plus capability). Move the oracle, RPC and InputBox policies into short '### Fee oracle' / '### RPC endpoint' subsections under 'Actors and trust', keeping the existing anchor target so links keep working. Line count is roughly neutral; words drop about 30%.

### N13. Fail-loud policy and divergence-check bounds restated across AGENTS, invariants, README; register layout drift

`low · confirmed · -45 lines · S · docs-only`  
Where: `AGENTS.md:331`, `docs/invariants.md:14-68`, `docs/invariants.md:32-59`, `docs/invariants.md:309-367`, `docs/invariants.md:452-473`

- Claim: AGENTS.md:331 restates the invariants.md check policy in a 10-line bullet. The policy section itself carries three admission/maintenance/selection bullets (28 lines) that restate the authority-boundary ADR, which they cite as owner. README.md:50-61 restates I9/I15's bounds. I15 (59 lines) has grown four sub-essays: runtime reaction, race bound, watchdog boundary, baseline-aware frontier. The Do-not-simplify list sits between I18 and I19 instead of after the register.
- Fix: Cut AGENTS.md:331 to 'Impossible states fail loud; never handle or absorb them. Decision test: docs/invariants.md.' Reduce invariants.md:32-59 to one line linking the ADR. Cut README.md:50-61 to 3 lines plus a link. Move Do-not-simplify below I20. Leave I15's substance alone (it is load-bearing) but turn its sub-bullets into terse 'holds/enforced/breaks' fragments.

### N14. Style-rule tweak to stop the drift: 'one fact, one home' plus size caps

`low · confirmed · +2 lines · S · docs-only`  
Where: `AGENTS.md:328-331`, `AGENTS.md:333-360`, `docs/review/register.md:62-63`, `docs/plans/2026-07-track3-feed-replay-design.md:63-64`

- Claim: The current rules forbid codenames and narration, and those hold. What drifts is repetition, length and defensive disclaimers. There are 30 'not a proof / does not establish / is not implied' phrases in living docs and the register, such as 'This guard cannot guarantee timely L1 inclusion.' and 'This is reference evidence, not private-engine conformance or a deployment benchmark.' Nothing in the rules caps rustdoc length or forbids restating an owner's lists.
- Fix: Replace AGENTS.md:328-329 with four rules. (1) One fact, one home: elsewhere, name it in a clause and link; never restate its conditions, orderings or lists. (2) Module header at most 4 lines and rustdoc contract at most about 6; longer reasoning goes to the owning doc or invariant, linked. (3) State a non-guarantee once, at its owner, only where a reader would otherwise assume the guarantee. (4) Table cells hold one sentence. Optionally add a warn-only CI awk check for //! headers over 6 lines and /// runs over 20 lines, about 10 lines of script. It protects only doc hygiene, so add it only if the drift recurs after the cleanup.
- Decision: Adopt the caps as review norms only, or also add the warn-only CI check?

### NS1. flush_and_wait rustdoc (35 lines) duplicates the recovery README's flush section

`low · skeptic-added · -28 lines · S`  
Where: `sequencer/src/recovery/flusher.rs:157-191`, `docs/recovery/README.md:155-174`

- Claim: The flush_and_wait rustdoc is the third-longest in production code. It restates the completion predicate, the no-op nonce range, the zombie-coverage rationale and the safe-block return contract. docs/recovery/README.md 'Flush the covered wallet slots' already owns all four, with the I14 link.
- Fix: Cut to about 6 lines: `/// Consume every covered wallet slot at safe depth with 0-ETH self-transfers;\n/// returns once Pending <= Safe && Safe >= watermark + 1 (the watermark conjunct\n/// covers zombies the local node forgot; I14). Returns the safe block of that\n/// observation: the caller must not cascade before its re-synced view reaches it.\n/// Procedure and rationale: docs/recovery/README.md "Flush the covered wallet slots".` Leave the loop and the watch-timeout outer retry unchanged; the Do-not-simplify rule on flusher retry loops concerns code, not this comment.

Keep (load-bearing):

- AGENTS.md:65-86 duality map (three implementations that must agree, drain attribution, empty-batch rule): the single most important baseline fact for avoiding scheduler/sequencer divergence.
- AGENTS.md:444-464 Reading Routes table: cheap routing that is what allows the rest of the baseline to shrink.
- AGENTS.md:402-427 Always/Ask First/Never: compact, and encodes the ask-first contract areas.
- docs/invariants.md register format (holds / enforced by / depended on by / breaks) and the Do-not-simplify list: the guard against locally sound refactors; move the list but keep its content.
- docs/invariants.md I15 substance (docs/invariants.md:309-367): race bound and watchdog boundary are load-bearing; compress the wording, not the facts.
- docs/protocol/scheduler-semantics.md algorithm steps and the count-transition table (:152-174): the canonical owner of acceptance semantics.
- docs/recovery/README.md 'Why the closed-suffix policy is safe' and the two current TLA+ models (preemptive.tla, admission.tla): the safety argument for flush-before-cascade.
- docs/recovery/cockroach.md checkpoint eligibility, flush and stopping-block sections: safety preconditions that survive the planned shared rebuild tool.
- docs/watchdog/incident-runbook.md and operator-deployment.md: operational procedures; the maintainer wants incident procedures in a runbook with tooling and drills.
- docs/review/README.md lifecycle plus register.md: the process works; it just needs applying (N8).
- docs/review/2026-09-25-sender-index-commit-cost.md: retained measurement with live consumers at register.md:207 and :236.
- sequencer/src/recovery/flusher.rs:157-191 flush_and_wait doc: explains the durable watermark anchor of the loop condition, which is non-obvious and safety-critical.
- sequencer/src/commands/run/workers.rs:20-26: explains that the explicit per-worker shape is compiler-enforced, the reason the components-vec redesign was refuted; trim it, keep the point.
- sequencer-core/src/protocol.rs:353-362 'Deliberately omitted structural rejects': deliberate absence recorded once at its seam, as the doc policy asks.
