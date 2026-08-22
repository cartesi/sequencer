----------------------------- MODULE admission -----------------------------
(*
 * Run-specific admission model for the startup recovery controller.
 *
 * Batch/L1 slot mechanics stay in preemptive.tla. This model checks the
 * controller protocol implemented by recovery/mod.rs and commands/run/:
 *
 *   inspect one local SQLite fact set -> classify once -> decide
 *       -> perform at most one phase -> inspect again
 *
 * The first clean decision grants no authority. It starts fallible, task-free
 * Prepare, whose successful completion also returns to local inspection. Only
 * a second clean decision creates AdmittedRuntime.
 *
 * Admission gating is fact-derived (review L2, 2026-08-19): there is no
 * lifecycle admission state machine and no acknowledgement step, and no
 * durable per-attempt record gates anything (L3, 2026-08-22 — the
 * terminal-fault black box is write-only telemetry outside this model).
 * Settlement and crash both end the attempt, and the next boot begins fresh
 * over whatever facts persist. Divergence and danger facts are durable and
 * survive attempts; everything else is boot-local.
 *
 * Flushed and PostFlushSynced abstract the non-clone session witnesses carried
 * by the Rust controller. They are ephemeral: crash, Retry, and Refuse erase
 * them, so another attempt must flush again before it can cascade.
 *)

EXTENDS TLC

Idle          == "Idle"
InspectLocal  == "InspectLocal"
Decide        == "Decide"
Prepare       == "Prepare"
InitialSync   == "InitialSync"
EnsureOpenTip == "EnsureOpenTip"
RecoverTip    == "RecoverTip"
Flush         == "Flush"
PostFlushSync == "PostFlushSync"
Cascade       == "Cascade"
Admitted      == "Admitted"

PhasePC == {InitialSync, EnsureOpenTip, RecoverTip, Flush, PostFlushSync,
             Cascade}
StartupPC == {InspectLocal, Decide, Prepare} \union PhasePC
ControllerStates == {Idle, Admitted} \union StartupPC

NoProgress       == "NoProgress"
NeedInitialSync  == "NeedInitialSync"
Inspecting       == "Inspecting"
Flushed          == "Flushed"
PostFlushSynced  == "PostFlushSynced"
Repaired         == "Repaired"

ProgressStates == {NoProgress, NeedInitialSync, Inspecting, Flushed,
                    PostFlushSynced, Repaired}

Safe         == "Safe"
ClosedDanger == "ClosedDanger"
TipDanger    == "TipDanger"
RetryDanger  == "RetryDanger"

DangerStates == {Safe, ClosedDanger, TipDanger, RetryDanger}

NoPostFlushView == "NoPostFlushView"
CaughtUp        == "CaughtUp"
Behind          == "Behind"
MissingSafeHead == "MissingSafeHead"

PostFlushViews == {CaughtUp, Behind, MissingSafeHead}

NoDecision == "NoDecision"
Admit       == "Admit"
Retry       == "Retry"
Refuse      == "Refuse"

Decisions == {NoDecision, Admit, Retry, Refuse} \union PhasePC

VARIABLES
    controller,
    admittedRuntime,
    prepared,
    progress,
    danger,
    hasFinalizedSnapshot,
    hasOpenTip,
    postFlushView,
    canonicalDivergence,
    decision,
    mustInspect

vars == <<controller, admittedRuntime, prepared, progress,
          danger, hasFinalizedSnapshot, hasOpenTip, postFlushView,
          canonicalDivergence, decision, mustInspect>>

HasFlushWitness == progress \in {Flushed, PostFlushSynced}
HasPostFlushSyncWitness == progress = PostFlushSynced

Reduce(currentProgress, currentDanger, snapshotPresent, tipPresent,
       currentPostFlushView, diverged) ==
    IF diverged \/ ~snapshotPresent
    THEN Refuse
    ELSE CASE currentProgress = NeedInitialSync -> InitialSync
         []   currentProgress = Flushed -> PostFlushSync
         []   currentProgress = PostFlushSynced ->
                  CASE currentPostFlushView = MissingSafeHead -> Refuse
                  []   currentPostFlushView = Behind -> Retry
                  []   OTHER -> Cascade
         []   currentProgress = Repaired ->
                  CASE currentDanger = Safe /\ tipPresent -> Admit
                  []   currentDanger = Safe -> EnsureOpenTip
                  []   OTHER -> Retry
         []   currentProgress = Inspecting ->
                  CASE currentDanger = Safe /\ tipPresent -> Admit
                  []   currentDanger = Safe -> EnsureOpenTip
                  []   currentDanger = ClosedDanger -> Flush
                  []   currentDanger = TipDanger -> RecoverTip
                  []   OTHER -> Retry
         []   OTHER -> Refuse

---------------------------------------------------------------------------
(* Initial state: a boot begins over any persisted fact shape, including
 * pre-existing divergence. TLC explores all locally inspectable fact
 * shapes. *)

Init ==
    /\ controller = Idle
    /\ admittedRuntime = FALSE
    /\ prepared = FALSE
    /\ progress = NoProgress
    /\ danger \in DangerStates
    /\ hasFinalizedSnapshot \in BOOLEAN
    /\ hasOpenTip \in BOOLEAN
    /\ postFlushView = NoPostFlushView
    /\ canonicalDivergence \in BOOLEAN
    /\ decision = NoDecision
    /\ mustInspect = FALSE

(* Retry/Refuse ends the attempt. Prepared resources and session witnesses do
 * not cross that boundary; the next attempt begins fresh. *)
Settle ==
    /\ controller' = Idle
    /\ admittedRuntime' = FALSE
    /\ prepared' = FALSE
    /\ progress' = NoProgress
    /\ postFlushView' = NoPostFlushView
    /\ decision' = NoDecision
    /\ mustInspect' = FALSE
    /\ UNCHANGED <<danger, hasFinalizedSnapshot, hasOpenTip,
                    canonicalDivergence>>

---------------------------------------------------------------------------
(* Attempt begin and the single local inspection step. Begin has no
 * lifecycle-state precondition (L2): the fact gates the code checks here —
 * two-sided setup completion — are outside this model's scope, and the
 * kernel process lock excludes a concurrent owner. *)

BeginRun ==
    /\ controller = Idle
    /\ controller' = InspectLocal
    /\ admittedRuntime' = FALSE
    /\ prepared' = FALSE
    /\ progress' = NeedInitialSync
    /\ postFlushView' = NoPostFlushView
    /\ decision' = NoDecision
    /\ mustInspect' = TRUE
    /\ UNCHANGED <<danger, hasFinalizedSnapshot, hasOpenTip,
                    canonicalDivergence>>

(* One SQLite RecoveryInspection is the only input to Reduce. Persisted
 * divergence and missing finalized state are classified by the same call. *)
InspectFacts ==
    /\ controller = InspectLocal
    /\ controller' = Decide
    /\ decision' = Reduce(progress, danger, hasFinalizedSnapshot,
                           hasOpenTip, postFlushView,
                           canonicalDivergence)
    /\ mustInspect' = FALSE
    /\ UNCHANGED <<admittedRuntime, prepared, progress, danger,
                    hasFinalizedSnapshot, hasOpenTip, postFlushView,
                    canonicalDivergence>>

(* Storage-open/query failures are centrally classified. A known local
 * divergence cannot be masked by the retry edge. *)
InspectRetry ==
    /\ controller = InspectLocal
    /\ ~canonicalDivergence
    /\ Settle

InspectRefuse ==
    /\ controller = InspectLocal
    /\ Settle

---------------------------------------------------------------------------
(* Decision handling before preparation. *)

DecidePhase ==
    /\ controller = Decide
    /\ ~prepared
    /\ decision \in PhasePC
    /\ controller' = decision
    /\ UNCHANGED <<admittedRuntime, prepared, progress, danger,
                    hasFinalizedSnapshot, hasOpenTip, postFlushView,
                    canonicalDivergence, decision, mustInspect>>

DecideRetry ==
    /\ controller = Decide
    /\ decision = Retry
    /\ Settle

DecideRefuse ==
    /\ controller = Decide
    /\ decision = Refuse
    /\ Settle

(* The first clean decision begins authority-neutral, task-free preparation. *)
BeginPrepare ==
    /\ controller = Decide
    /\ ~prepared
    /\ decision = Admit
    /\ controller' = Prepare
    /\ UNCHANGED <<admittedRuntime, prepared, progress, danger,
                    hasFinalizedSnapshot, hasOpenTip, postFlushView,
                    canonicalDivergence, decision, mustInspect>>

(* If the final inspection no longer says Admit, prepared resources are
 * dropped and the attempt exits; no new recovery phase runs on the aged
 * prepared state. *)
PreparedDecisionChanged ==
    /\ controller = Decide
    /\ prepared
    /\ decision \in PhasePC
    /\ Settle

(* The capability boundary: the final clean decision and AdmittedRuntime are
 * one atomic action; launch consumes the capability without yielding. *)
AdmitRuntime ==
    /\ controller = Decide
    /\ prepared
    /\ decision = Admit
    /\ controller' = Admitted
    /\ admittedRuntime' = TRUE
    /\ UNCHANGED <<prepared, progress, danger, hasFinalizedSnapshot,
                    hasOpenTip, postFlushView, canonicalDivergence,
                    decision, mustInspect>>

---------------------------------------------------------------------------
(* Recovery phase completion. Every successful phase returns to InspectLocal.
 * InitialSync and PostFlushSync may update observed danger facts; either Sync
 * may also discover canonical divergence. *)

CompletePhase(nextProgress, nextDanger, nextTipPresent, nextPostFlushView,
              discoversDivergence) ==
    /\ controller \in PhasePC
    /\ ~prepared
    /\ ~canonicalDivergence
    /\ controller' = InspectLocal
    /\ progress' = nextProgress
    /\ danger' = nextDanger
    /\ hasOpenTip' = nextTipPresent
    /\ postFlushView' = nextPostFlushView
    /\ canonicalDivergence' = discoversDivergence
    /\ decision' = NoDecision
    /\ mustInspect' = TRUE
    /\ UNCHANGED <<admittedRuntime, prepared, hasFinalizedSnapshot>>

InitialSyncCompleted ==
    /\ controller = InitialSync
    /\ \E nextDanger \in DangerStates:
        CompletePhase(Inspecting, nextDanger, hasOpenTip,
                      NoPostFlushView, FALSE)

EnsureOpenTipCompleted ==
    /\ controller = EnsureOpenTip
    /\ CompletePhase(Repaired, Safe, TRUE, NoPostFlushView, FALSE)

(* The two repair completions restrict post-repair facts deliberately: these
 * are faithful storage postconditions, not narrowing. `hasOpenTip' = TRUE`
 * because `recover_aging_tip_for_recovery` / `cascade_and_reopen` end with a
 * valid open batch in the same transaction (storage/recovery.rs). Danger is
 * `{Safe, RetryDanger}`: RecoverTip fires only after the closed frontier was
 * checked clean and its cascade touches only `>= tip`, Cascade invalidates
 * the whole non-gold closed suffix, the fresh tip's first frame carries the
 * current safe block, and no repair phase contacts L1 — so ClosedDanger /
 * TipDanger cannot reappear and only the retryable observations remain.
 * Widening either action would model states the implementation cannot
 * produce (2026-08-18 review, D8). *)
RecoverTipCompleted ==
    /\ controller = RecoverTip
    /\ \E nextDanger \in {Safe, RetryDanger}:
        CompletePhase(Repaired, nextDanger, TRUE,
                      NoPostFlushView, FALSE)

FlushCompleted ==
    /\ controller = Flush
    /\ CompletePhase(Flushed, danger, hasOpenTip,
                     NoPostFlushView, FALSE)

PostFlushSyncCompleted ==
    /\ controller = PostFlushSync
    /\ \E nextDanger \in DangerStates:
        \E nextView \in PostFlushViews:
            CompletePhase(PostFlushSynced, nextDanger, hasOpenTip,
                          nextView, FALSE)

CascadeCompleted ==
    /\ controller = Cascade
    /\ \E nextDanger \in {Safe, RetryDanger}:
        CompletePhase(Repaired, nextDanger, TRUE,
                      NoPostFlushView, FALSE)

SyncDiscoversDivergence ==
    \/ /\ controller = InitialSync
       /\ \E nextDanger \in DangerStates:
           CompletePhase(Inspecting, nextDanger, hasOpenTip,
                         NoPostFlushView, TRUE)
    \/ /\ controller = PostFlushSync
       /\ \E nextDanger \in DangerStates:
           \E nextView \in PostFlushViews:
               CompletePhase(PostFlushSynced, nextDanger, hasOpenTip,
                             nextView, TRUE)

PhaseRetry ==
    /\ controller \in PhasePC
    /\ ~canonicalDivergence
    /\ Settle

PhaseRefuse ==
    /\ controller \in PhasePC
    /\ ~canonicalDivergence
    /\ Settle

---------------------------------------------------------------------------
(* Fallible task-free preparation. Time may pass, so the next inspection may
 * derive a different danger status even though preparation changes no local
 * recovery fact itself. *)

PrepareCompleted ==
    /\ controller = Prepare
    /\ ~prepared
    /\ decision = Admit
    /\ \E nextDanger \in DangerStates:
        /\ controller' = InspectLocal
        /\ prepared' = TRUE
        /\ progress' = Inspecting
        /\ danger' = nextDanger
        /\ decision' = NoDecision
        /\ mustInspect' = TRUE
        /\ UNCHANGED <<admittedRuntime, hasFinalizedSnapshot,
                        hasOpenTip, postFlushView,
                        canonicalDivergence>>

PrepareRetry ==
    /\ controller = Prepare
    /\ ~canonicalDivergence
    /\ Settle

PrepareRefuse ==
    /\ controller = Prepare
    /\ ~canonicalDivergence
    /\ Settle

---------------------------------------------------------------------------
(* Crash destroys PreparedRuntime, AdmittedRuntime, and session witnesses.
 * Nothing durable gates the next boot (L2/L3 — the terminal-fault black box
 * is write-only telemetry), so a restart is simply a fresh attempt over the
 * surviving durable facts. Modeled as returning directly to the pre-begin
 * shape with facts unchanged. *)

Crash ==
    /\ controller \in StartupPC \union {Admitted}
    /\ Settle

CleanShutdown ==
    /\ controller = Admitted
    /\ admittedRuntime
    /\ Settle

---------------------------------------------------------------------------
(* Safety invariants. *)

TypeOK ==
    /\ controller \in ControllerStates
    /\ admittedRuntime \in BOOLEAN
    /\ prepared \in BOOLEAN
    /\ progress \in ProgressStates
    /\ danger \in DangerStates
    /\ hasFinalizedSnapshot \in BOOLEAN
    /\ hasOpenTip \in BOOLEAN
    /\ postFlushView \in PostFlushViews \union {NoPostFlushView}
    /\ canonicalDivergence \in BOOLEAN
    /\ decision \in Decisions
    /\ mustInspect \in BOOLEAN

ControllerShape ==
    /\ controller = Idle =>
        /\ ~admittedRuntime
        /\ ~prepared
        /\ progress = NoProgress
    /\ controller \in StartupPC => ~admittedRuntime

ProgressShape ==
    /\ controller \in StartupPC => progress \in ProgressStates \ {NoProgress}
    /\ controller = Admitted => progress = Inspecting
    /\ postFlushView # NoPostFlushView <=> HasPostFlushSyncWitness
    /\ prepared =>
        /\ progress = Inspecting
        /\ controller \in {InspectLocal, Decide, Admitted}

AdmittedRuntimeSound ==
    admittedRuntime =>
        /\ controller = Admitted
        /\ prepared
        /\ progress = Inspecting
        /\ decision = Admit
        /\ decision = Reduce(progress, danger, hasFinalizedSnapshot,
                             hasOpenTip, postFlushView,
                             canonicalDivergence)
        /\ danger = Safe
        /\ hasFinalizedSnapshot
        /\ hasOpenTip
        /\ ~canonicalDivergence

AdmissionIsAtomic ==
    controller = Admitted => admittedRuntime

MandatoryReinspection ==
    mustInspect =>
        /\ controller = InspectLocal
        /\ ~admittedRuntime

ClassifiedOnce ==
    controller = Decide =>
        decision = Reduce(progress, danger, hasFinalizedSnapshot,
                          hasOpenTip, postFlushView,
                          canonicalDivergence)

EphemeralWitnessScope ==
    /\ HasPostFlushSyncWitness => HasFlushWitness
    /\ HasFlushWitness =>
        /\ ~prepared
        /\ controller \in StartupPC
    /\ controller = PostFlushSync => progress = Flushed
    /\ controller = Cascade =>
        /\ progress = PostFlushSynced
        /\ postFlushView = CaughtUp

LocalDivergenceFirst ==
    /\ canonicalDivergence => ~admittedRuntime
    /\ canonicalDivergence /\ controller = Decide => decision = Refuse
    /\ controller \in ({Prepare, Admitted} \union PhasePC) =>
        ~canonicalDivergence

PhasePreconditions ==
    /\ controller \in PhasePC => decision = controller
    /\ controller = InitialSync => progress = NeedInitialSync
    /\ controller = EnsureOpenTip =>
        /\ progress \in {Inspecting, Repaired}
        /\ danger = Safe
        /\ ~hasOpenTip
    /\ controller = RecoverTip =>
        /\ progress = Inspecting
        /\ danger = TipDanger
    /\ controller = Flush =>
        /\ progress = Inspecting
        /\ danger = ClosedDanger

PrepareRequiresFirstCleanInspection ==
    controller = Prepare =>
        /\ ~prepared
        /\ progress \in {Inspecting, Repaired}
        /\ decision = Admit
        /\ danger = Safe
        /\ hasFinalizedSnapshot
        /\ hasOpenTip
        /\ ~canonicalDivergence

Inv ==
    /\ TypeOK
    /\ ControllerShape
    /\ ProgressShape
    /\ AdmittedRuntimeSound
    /\ AdmissionIsAtomic
    /\ MandatoryReinspection
    /\ ClassifiedOnce
    /\ EphemeralWitnessScope
    /\ LocalDivergenceFirst
    /\ PhasePreconditions
    /\ PrepareRequiresFirstCleanInspection

---------------------------------------------------------------------------

Next ==
    \/ BeginRun
    \/ InspectFacts
    \/ InspectRetry
    \/ InspectRefuse
    \/ DecidePhase
    \/ DecideRetry
    \/ DecideRefuse
    \/ BeginPrepare
    \/ PreparedDecisionChanged
    \/ AdmitRuntime
    \/ InitialSyncCompleted
    \/ EnsureOpenTipCompleted
    \/ RecoverTipCompleted
    \/ FlushCompleted
    \/ PostFlushSyncCompleted
    \/ CascadeCompleted
    \/ SyncDiscoversDivergence
    \/ PhaseRetry
    \/ PhaseRefuse
    \/ PrepareCompleted
    \/ PrepareRetry
    \/ PrepareRefuse
    \/ Crash
    \/ CleanShutdown

Spec == Init /\ [][Next]_vars

=============================================================================
