----------------------------- MODULE admission -----------------------------
(*
 * Sequential startup while a kernel lock excludes other writers and no
 * runtime worker exists. Batch/slot mechanics are modeled in preemptive.tla.
 *
 * Local gate -> initial Sync -> select at most one repair -> clean check
 * -> task-free Prepare -> final fresh check -> atomic runtime admission.
 * Closed recovery is Flush -> Sync -> guarded Cascade, regardless of whether
 * the refreshed danger verdict still calls for recovery. Only Sync can
 * discover canonical divergence. Flush changes no local recovery fact.
 *
 * The flush observation and post-flush view are boot-local. Retry, refusal,
 * and crash erase them; another attempt must flush again before cascading.
 * Persisted history and danger facts survive. Terminal-fault telemetry does
 * not gate admission and is outside the model.
 *)

EXTENDS TLC

Idle          == "Idle"
LocalGate     == "LocalGate"
InitialSync   == "InitialSync"
SelectRepair  == "SelectRepair"
EnsureOpenTip == "EnsureOpenTip"
RecoverTip    == "RecoverTip"
Flush         == "Flush"
PostFlushSync == "PostFlushSync"
Cascade       == "Cascade"
CheckRepair   == "CheckRepair"
Prepare       == "Prepare"
FinalCheck    == "FinalCheck"
Admitted      == "Admitted"

StartupPC == {LocalGate, InitialSync, SelectRepair, EnsureOpenTip, RecoverTip,
              Flush, PostFlushSync, Cascade, CheckRepair, Prepare, FinalCheck}
ControllerStates == {Idle, Admitted} \union StartupPC

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

VARIABLES controller, admittedRuntime, prepared, flushed, postFlushView,
          danger, hasFinalizedSnapshot, hasOpenTip, canonicalDivergence

vars == <<controller, admittedRuntime, prepared, flushed, postFlushView,
          danger, hasFinalizedSnapshot, hasOpenTip, canonicalDivergence>>

LocalTerminal == canonicalDivergence \/ ~hasFinalizedSnapshot
Clean == ~LocalTerminal /\ danger = Safe /\ hasOpenTip

Init ==
    /\ controller = Idle
    /\ admittedRuntime = FALSE
    /\ prepared = FALSE
    /\ flushed = FALSE
    /\ postFlushView = NoPostFlushView
    /\ danger \in DangerStates
    /\ hasFinalizedSnapshot \in BOOLEAN
    /\ hasOpenTip \in BOOLEAN
    /\ canonicalDivergence \in BOOLEAN

Settle ==
    /\ controller' = Idle
    /\ admittedRuntime' = FALSE
    /\ prepared' = FALSE
    /\ flushed' = FALSE
    /\ postFlushView' = NoPostFlushView
    /\ UNCHANGED <<danger, hasFinalizedSnapshot, hasOpenTip, canonicalDivergence>>

MoveTo(next) ==
    /\ controller' = next
    /\ UNCHANGED <<admittedRuntime, prepared, flushed, postFlushView,
                    danger, hasFinalizedSnapshot, hasOpenTip, canonicalDivergence>>

BeginRun ==
    /\ controller = Idle
    /\ MoveTo(LocalGate)

CheckLocalGate ==
    /\ controller = LocalGate
    /\ IF LocalTerminal THEN Settle ELSE MoveTo(InitialSync)

(* Successful sync writes one atomic input/safe-head observation. Only these
 * two calls can discover new divergence; all other startup actions preserve
 * it. Initial provider failure may instead use the persisted local view. *)
SyncCompleted ==
    /\ controller \in {InitialSync, PostFlushSync}
    /\ ~LocalTerminal
    /\ \E nextDanger \in DangerStates, diverged \in BOOLEAN:
        /\ danger' = nextDanger
        /\ canonicalDivergence' = diverged
        /\ IF controller = InitialSync
           THEN /\ controller' = SelectRepair
                /\ postFlushView' = NoPostFlushView
           ELSE /\ controller' = Cascade
                /\ postFlushView' \in PostFlushViews
        /\ UNCHANGED <<admittedRuntime, prepared, flushed,
                        hasFinalizedSnapshot, hasOpenTip>>

InitialProviderFailure ==
    /\ controller = InitialSync
    /\ MoveTo(SelectRepair)

SelectAction ==
    /\ controller = SelectRepair
    /\ IF LocalTerminal \/ danger = RetryDanger
       THEN Settle
       ELSE CASE danger = Safe /\ hasOpenTip -> MoveTo(Prepare)
            []   danger = Safe -> MoveTo(EnsureOpenTip)
            []   danger = TipDanger -> MoveTo(RecoverTip)
            []   danger = ClosedDanger -> MoveTo(Flush)

FlushCompleted ==
    /\ controller = Flush
    /\ controller' = PostFlushSync
    /\ flushed' = TRUE
    /\ UNCHANGED <<admittedRuntime, prepared, postFlushView, danger,
                    hasFinalizedSnapshot, hasOpenTip, canonicalDivergence>>

(* Repair methods commit an open Tip in the same transaction as their
 * mutation. No L1 observation changes, so observed danger cannot reappear;
 * elapsed time or a clock fault can still leave RetryDanger. *)
CommitRepair ==
    /\ controller' = CheckRepair
    /\ hasOpenTip' = TRUE
    /\ danger' \in {Safe, RetryDanger}
    /\ UNCHANGED <<admittedRuntime, prepared, flushed, postFlushView,
                    hasFinalizedSnapshot, canonicalDivergence>>

LocalRepairCompleted ==
    /\ controller \in {EnsureOpenTip, RecoverTip}
    /\ ~LocalTerminal
    /\ CommitRepair

(* The cascade transaction itself checks new terminal facts and the observed
 * flush floor. Even a now-Safe view must take this branch: the flushed suffix
 * may contain young unresolved batches that no longer trigger danger. *)
GuardedCascade ==
    /\ controller = Cascade
    /\ IF LocalTerminal \/ postFlushView # CaughtUp
       THEN Settle
       ELSE CommitRepair

CheckRepairCompleted ==
    /\ controller = CheckRepair
    /\ IF Clean THEN MoveTo(Prepare) ELSE Settle

(* Preparation changes no durable recovery fact. Its duration can invalidate
 * a previously clean view, so final admission must check current danger. *)
PrepareCompleted ==
    /\ controller = Prepare
    /\ controller' = FinalCheck
    /\ prepared' = TRUE
    /\ danger' \in {Safe, RetryDanger}
    /\ UNCHANGED <<admittedRuntime, flushed, postFlushView,
                    hasFinalizedSnapshot, hasOpenTip, canonicalDivergence>>

FinalAdmission ==
    /\ controller = FinalCheck
    /\ IF Clean
       THEN /\ controller' = Admitted
            /\ admittedRuntime' = TRUE
            /\ UNCHANGED <<prepared, flushed, postFlushView, danger,
                            hasFinalizedSnapshot, hasOpenTip, canonicalDivergence>>
       ELSE Settle

(* Typed I/O/guard failures terminate the attempt. In particular, post-flush
 * Sync has no provider-failure fallback. Guard time can age the selected
 * Safe/TipDanger state before the corresponding local write. *)
OperationFailed ==
    /\ controller \in StartupPC
    /\ Settle

Crash ==
    /\ controller \in StartupPC \union {Admitted}
    /\ Settle

CleanShutdown ==
    /\ controller = Admitted
    /\ Settle

---------------------------------------------------------------------------
(* Semantic safety properties, independent of how inspections are factored. *)

TypeOK ==
    /\ controller \in ControllerStates
    /\ admittedRuntime \in BOOLEAN
    /\ prepared \in BOOLEAN
    /\ flushed \in BOOLEAN
    /\ postFlushView \in PostFlushViews \union {NoPostFlushView}
    /\ danger \in DangerStates
    /\ hasFinalizedSnapshot \in BOOLEAN
    /\ hasOpenTip \in BOOLEAN
    /\ canonicalDivergence \in BOOLEAN

AdmittedRuntimeSound ==
    admittedRuntime => controller = Admitted /\ prepared /\ Clean

AdmissionIsAtomic == (controller = Admitted) = admittedRuntime

EphemeralWitnessScope ==
    /\ controller \in {Idle, LocalGate, InitialSync, SelectRepair, Flush}
        => ~flushed /\ postFlushView = NoPostFlushView
    /\ controller = PostFlushSync => flushed /\ postFlushView = NoPostFlushView
    /\ controller = Cascade => flushed /\ postFlushView \in PostFlushViews
    /\ postFlushView # NoPostFlushView => flushed
    /\ controller = Idle => ~prepared /\ ~admittedRuntime

LocalTerminalDominance ==
    /\ LocalTerminal => ~admittedRuntime
    /\ controller \in {InitialSync, EnsureOpenTip, RecoverTip, Flush,
                         PostFlushSync, Prepare, FinalCheck, Admitted}
        => ~LocalTerminal

PostFlushRepairSound ==
    flushed /\ controller \in {CheckRepair, Prepare, FinalCheck, Admitted}
        => postFlushView = CaughtUp

RepairPreconditions ==
    /\ controller = EnsureOpenTip => danger = Safe /\ ~hasOpenTip
    /\ controller = RecoverTip => danger = TipDanger
    /\ controller = Flush => danger = ClosedDanger
    /\ controller = CheckRepair => hasOpenTip /\ ~LocalTerminal
    /\ controller = Prepare => Clean /\ ~prepared
    /\ prepared => controller \in {FinalCheck, Admitted}

Inv == TypeOK /\ AdmittedRuntimeSound /\ AdmissionIsAtomic
       /\ EphemeralWitnessScope /\ LocalTerminalDominance /\ PostFlushRepairSound
       /\ RepairPreconditions

Next == BeginRun \/ CheckLocalGate \/ SyncCompleted \/ InitialProviderFailure
        \/ SelectAction \/ FlushCompleted \/ LocalRepairCompleted \/ GuardedCascade
        \/ CheckRepairCompleted \/ PrepareCompleted \/ FinalAdmission
        \/ OperationFailed \/ Crash \/ CleanShutdown

Spec == Init /\ [][Next]_vars

=============================================================================
