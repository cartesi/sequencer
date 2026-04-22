---------------------------- MODULE preemptive -----------------------------
(*
 * Full operational model of the preemptive recovery design.
 *
 * Extends V3 with flush modeling: at each w_nonce slot, L1
 * non-deterministically includes the spine batch OR a flush no-op
 * (killing the batch).  This captures the complete flush lifecycle
 * including the case where the frontier batch itself is killed.
 *
 * A killed batch acts as silent poison: the scheduler never sees it,
 * so schedulerExpected stays stuck at its batch_nonce.  All subsequent
 * batches — whether alive on L1 or dead — have wrong nonces.
 * Recovery resubmits the killed batch; if stale by inclusion, Resolve
 * cascades; if fresh, the scheduler accepts it.  Resolve can also
 * discard an aging open Tip whose current-safe-block age has reached
 * MAX_WAIT_BLOCKS.
 *
 * Colors on the spine: Gold* Silver* Bronze* Pending* Tip
 * During flush, SpineOrdering can be temporarily violated (a killed
 * Pending appears before a surviving Silver).  This is transient —
 * recovery restores Gold* + Tip.  SpineOrdering is NOT checked as
 * an invariant.
 *
 * Proves: ZombieSafety == schedulerExpected = CountGold(spine)
 *
 * Actions:
 *   AdvanceTip       -- close tip -> Pending, append new Tip
 *   SubmitBatch      -- assign w_nonces to unsubmitted Pendings
 *   L1IncludeSpine   -- spine batch wins its slot -> Bronze/Silver
 *   L1SkipSpine      -- flush no-op wins, spine batch killed
 *   L1IncludeDead    -- dead batch beats its flush no-op
 *   L1SkipDead       -- flush no-op wins, dead batch killed
 *   AdvanceSafeBlock -- L1 safe block advances, Bronze -> Silver
 *   SchedulerStep    -- scheduler processes next safe entry -> Gold
 *   SchedulerSkip    -- scheduler skips gap (no-op slot)
 *   Resolve          -- stale unresolved frontier -> cascade, recover
 *)

EXTENDS Integers, Sequences, FiniteSets

CONSTANTS
    MaxBatchIndex,
    MaxSafeBlock,
    MAX_WAIT_BLOCKS,
    MaxWalletNonce      \* bound on wallet nonce to keep state space finite

NONE == -1

---------------------------------------------------------------------------
(* Colors *)

Gold    == "Gold"
Silver  == "Silver"
Bronze  == "Bronze"
Pending == "Pending"
Tip     == "Tip"

Colors == {Gold, Silver, Bronze, Pending, Tip}

ColorOrd(c) ==
    CASE c = Gold    -> 0
    []   c = Silver  -> 1
    []   c = Bronze  -> 2
    []   c = Pending -> 3
    []   c = Tip     -> 4

---------------------------------------------------------------------------
(* Variables *)

VARIABLES
    spine,
    invalid,
    nextIndex,
    currentSafeBlock,
    walletNonce,
    nextL1Slot,
    l1Included,
    schedulerCursor,
    schedulerExpected,
    deadBatches

vars == <<spine, invalid, nextIndex, currentSafeBlock,
          walletNonce, nextL1Slot, l1Included,
          schedulerCursor, schedulerExpected, deadBatches>>

---------------------------------------------------------------------------
(* Helpers *)

CountGold(s) == Cardinality({i \in 1..Len(s) : s[i].color = Gold})

FirstNonGold(s) ==
    IF \E i \in 1..Len(s) : s[i].color # Gold
    THEN CHOOSE i \in 1..Len(s) :
            s[i].color # Gold /\ \A j \in 1..i-1 : s[j].color = Gold
    ELSE 0

PendingAtWNonce(s, wn) ==
    IF \E i \in 1..Len(s) : s[i].color = Pending /\ s[i].w_nonce = wn
    THEN CHOOSE i \in 1..Len(s) : s[i].color = Pending /\ s[i].w_nonce = wn
    ELSE 0

SilverAtBN(s, bn) ==
    IF \E i \in 1..Len(s) : s[i].color = Silver /\ s[i].batch_nonce = bn
    THEN CHOOSE i \in 1..Len(s) : s[i].color = Silver /\ s[i].batch_nonce = bn
    ELSE 0

---------------------------------------------------------------------------
(* Staleness *)

IsStaleByInclusion(b) == b.inclusion_block - b.safe_block >= MAX_WAIT_BLOCKS

IsStaleByCurrent(b) == currentSafeBlock - b.safe_block >= MAX_WAIT_BLOCKS

---------------------------------------------------------------------------
(* Invariants *)

TypeOK ==
    /\ Len(spine) >= 1
    /\ nextIndex \in Nat
    /\ currentSafeBlock \in Nat
    /\ walletNonce \in Nat
    /\ nextL1Slot \in Nat
    /\ schedulerCursor \in Nat
    /\ schedulerExpected \in Nat

\* Batch nonces are contiguous (0..N-1) for non-Tip spine elements.
BatchNoncesContiguous ==
    \A i \in 1..Len(spine) :
        spine[i].color # Tip => spine[i].batch_nonce = i - 1

\* Dead branches only hang off Gold nodes.
InvalidOnlyOnGold ==
    \A i \in 1..Len(spine) : invalid[i] > 0 => spine[i].color = Gold

\* ------- THE KEY THEOREM -------
\* The scheduler accepts exactly the Gold prefix.
ZombieSafety == schedulerExpected = CountGold(spine)

\* L1 consistency
L1WNonceUnique ==
    \A e1, e2 \in l1Included : e1.w_nonce = e2.w_nonce => e1 = e2

L1BeforeCursor ==
    \A e \in l1Included : e.w_nonce < nextL1Slot

SchedulerBehindL1 ==
    schedulerCursor <= nextL1Slot

DeadNotYetIncluded ==
    \A d \in deadBatches : d.w_nonce >= nextL1Slot

Inv ==
    /\ TypeOK
    /\ BatchNoncesContiguous
    /\ InvalidOnlyOnGold
    /\ ZombieSafety
    /\ L1WNonceUnique
    /\ L1BeforeCursor
    /\ SchedulerBehindL1
    /\ DeadNotYetIncluded

---------------------------------------------------------------------------
(* Initial state *)

(*
 * Initial state: Genesis sentinel (nonce 0) is already Gold.
 * This is a modeling technique that eliminates the nonce-0 edge
 * case, allowing Resolve to use uniform logic.  The implementation
 * can handle nonce-0 however is simplest (see README.md).
 *
 * Tip.safe_block models the first frame's safe_block of the open batch.
 * Keeping it meaningful lets the spec represent a Tip that ages past
 * MAX_WAIT_BLOCKS before ever getting an L1 transaction.
 *)
Init ==
    /\ spine = <<[index |-> 0, color |-> Gold, safe_block |-> 0,
                  inclusion_block |-> 0, w_nonce |-> 0, batch_nonce |-> 0],
                 [index |-> 1, color |-> Tip, safe_block |-> 0,
                  inclusion_block |-> 0, w_nonce |-> NONE, batch_nonce |-> 0]>>
    /\ invalid = <<0, 0>>
    /\ nextIndex = 2
    /\ currentSafeBlock = 0
    /\ walletNonce = 1
    /\ nextL1Slot = 1
    /\ l1Included = {[batch_nonce |-> 0, w_nonce |-> 0,
                      inclusion_block |-> 0, safe_block |-> 0,
                      is_safe |-> TRUE]}
    /\ schedulerCursor = 1
    /\ schedulerExpected = 1
    /\ deadBatches = {}

---------------------------------------------------------------------------
(* AdvanceTip: close tip -> Pending, append new Tip *)

AdvanceTip ==
    /\ nextIndex <= MaxBatchIndex
    /\ LET tipPos == Len(spine) IN
       /\ spine[tipPos].color = Tip
       /\ spine[tipPos].safe_block <= currentSafeBlock
       /\ (tipPos > 1 => spine[tipPos].safe_block >= spine[tipPos - 1].safe_block)
       /\ spine' = [i \in 1..Len(spine) + 1 |->
            IF i < tipPos THEN spine[i]
            ELSE IF i = tipPos
                 THEN [index          |-> spine[tipPos].index,
                       color          |-> Pending,
                       safe_block     |-> spine[tipPos].safe_block,
                       inclusion_block |-> 0,
                       w_nonce        |-> NONE,
                       batch_nonce    |-> tipPos - 1]
                 ELSE [index          |-> nextIndex,
                       color          |-> Tip,
                       safe_block     |-> currentSafeBlock,
                       inclusion_block |-> 0,
                       w_nonce        |-> NONE,
                       batch_nonce    |-> 0]]
       /\ invalid' = [i \in 1..Len(spine) + 1 |->
                          IF i <= Len(spine) THEN invalid[i] ELSE 0]
       /\ nextIndex' = nextIndex + 1
       /\ UNCHANGED <<currentSafeBlock, walletNonce, nextL1Slot,
                      l1Included, schedulerCursor, schedulerExpected,
                      deadBatches>>

---------------------------------------------------------------------------
(*
 * SubmitBatch: assign w_nonces to ALL unsubmitted Pending batches
 * at once, in spine-position order.
 *)
SubmitBatch ==
    LET unsubPos == {i \in 1..Len(spine) :
                        spine[i].color = Pending /\ spine[i].w_nonce = NONE}
        wn0 == IF walletNonce >= nextL1Slot THEN walletNonce ELSE nextL1Slot
    IN
    /\ unsubPos # {}
    /\ wn0 + Cardinality(unsubPos) <= MaxWalletNonce  \* bound check
    /\ spine' = [i \in 1..Len(spine) |->
                  IF i \in unsubPos
                  THEN [spine[i] EXCEPT
                          !.w_nonce = wn0 + Cardinality({j \in unsubPos : j < i})]
                  ELSE spine[i]]
    /\ walletNonce' = wn0 + Cardinality(unsubPos)
    /\ UNCHANGED <<invalid, nextIndex, currentSafeBlock, nextL1Slot,
                   l1Included, schedulerCursor, schedulerExpected,
                   deadBatches>>

---------------------------------------------------------------------------
(*
 * L1 actions: the L1 stream processes transactions in w_nonce order.
 * At each slot, if both a spine batch and a flush no-op exist,
 * L1 non-deterministically picks one.
 *
 * inclusion_block >= currentSafeBlock (L1 monotonicity) and
 * >= all previous inclusion_blocks (block ordering).
 *)

\* Spine batch wins its slot -> Bronze or Silver.
L1IncludeSpine ==
    LET pos == PendingAtWNonce(spine, nextL1Slot) IN
    /\ pos > 0
    /\ \E ib \in currentSafeBlock..MaxSafeBlock :
        /\ \A e \in l1Included : ib >= e.inclusion_block
        /\ LET isSafe   == ib <= currentSafeBlock
               newColor == IF isSafe THEN Silver ELSE Bronze
           IN
           /\ spine' = [spine EXCEPT ![pos].color = newColor,
                                     ![pos].inclusion_block = ib]
           /\ l1Included' = l1Included \union
                 {[batch_nonce    |-> spine[pos].batch_nonce,
                   w_nonce        |-> nextL1Slot,
                   inclusion_block |-> ib,
                   safe_block     |-> spine[pos].safe_block,
                   is_safe        |-> isSafe]}
           /\ nextL1Slot' = nextL1Slot + 1
           /\ UNCHANGED <<invalid, nextIndex, currentSafeBlock,
                          walletNonce, schedulerCursor,
                          schedulerExpected, deadBatches>>

\* Flush no-op wins at a spine Pending's slot.
\* The batch is killed: w_nonce reset to NONE.
\* The scheduler never sees it — silent nonce poison.
L1SkipSpine ==
    LET pos == PendingAtWNonce(spine, nextL1Slot) IN
    /\ pos > 0
    /\ spine' = [spine EXCEPT ![pos].w_nonce = NONE]
    /\ nextL1Slot' = nextL1Slot + 1
    /\ UNCHANGED <<invalid, nextIndex, currentSafeBlock,
                   walletNonce, l1Included, schedulerCursor,
                   schedulerExpected, deadBatches>>

\* Dead batch (from cascade) beats its flush no-op.
L1IncludeDead ==
    /\ \E d \in deadBatches : d.w_nonce = nextL1Slot
    /\ LET d == CHOOSE dd \in deadBatches : dd.w_nonce = nextL1Slot IN
       \E ib \in currentSafeBlock..MaxSafeBlock :
        /\ \A e \in l1Included : ib >= e.inclusion_block
        /\ l1Included' = l1Included \union
              {[batch_nonce    |-> d.batch_nonce,
                w_nonce        |-> nextL1Slot,
                inclusion_block |-> ib,
                safe_block     |-> d.safe_block,
                is_safe        |-> (ib <= currentSafeBlock)]}
        /\ deadBatches' = deadBatches \ {d}
        /\ nextL1Slot' = nextL1Slot + 1
        /\ UNCHANGED <<spine, invalid, nextIndex, currentSafeBlock,
                       walletNonce, schedulerCursor, schedulerExpected>>

\* Flush no-op wins at a dead batch's slot.
L1SkipDead ==
    /\ \E d \in deadBatches : d.w_nonce = nextL1Slot
    /\ LET d == CHOOSE dd \in deadBatches : dd.w_nonce = nextL1Slot IN
       /\ deadBatches' = deadBatches \ {d}
       /\ nextL1Slot' = nextL1Slot + 1
       /\ UNCHANGED <<spine, invalid, nextIndex, currentSafeBlock,
                      walletNonce, l1Included, schedulerCursor,
                      schedulerExpected>>

L1Include ==
    \/ L1IncludeSpine
    \/ L1SkipSpine
    \/ L1IncludeDead
    \/ L1SkipDead

---------------------------------------------------------------------------
(* AdvanceSafeBlock: L1 safe block advances, Bronze -> Silver *)

AdvanceSafeBlock ==
    /\ currentSafeBlock < MaxSafeBlock
    /\ \E sb \in (currentSafeBlock + 1)..MaxSafeBlock :
        /\ currentSafeBlock' = sb
        /\ spine' = [i \in 1..Len(spine) |->
                      IF spine[i].color = Bronze /\ spine[i].inclusion_block <= sb
                      THEN [spine[i] EXCEPT !.color = Silver]
                      ELSE spine[i]]
        /\ l1Included' = {[e EXCEPT !.is_safe =
                              (e.is_safe \/ (e.inclusion_block <= sb))]
                          : e \in l1Included}
        /\ UNCHANGED <<invalid, nextIndex, walletNonce, nextL1Slot,
                       schedulerCursor, schedulerExpected, deadBatches>>

---------------------------------------------------------------------------
(*
 * SchedulerStep: process the L1 entry at schedulerCursor.
 * Accept: batch_nonce matches AND not stale -> Gold promotion.
 * Skip: nonce mismatch OR stale (nonce poisoning).
 *)
SchedulerStep ==
    /\ \E e \in l1Included : e.w_nonce = schedulerCursor /\ e.is_safe
    /\ LET entry == CHOOSE e \in l1Included :
                        e.w_nonce = schedulerCursor /\ e.is_safe
       IN
       LET stale    == entry.inclusion_block - entry.safe_block
                       >= MAX_WAIT_BLOCKS
           accepted == entry.batch_nonce = schedulerExpected /\ ~stale
       IN
       /\ schedulerCursor' = schedulerCursor + 1
       /\ IF accepted
          THEN /\ schedulerExpected' = schedulerExpected + 1
               /\ LET gp == SilverAtBN(spine, schedulerExpected)
                  IN IF gp > 0
                     THEN spine' = [spine EXCEPT ![gp].color = Gold]
                     ELSE UNCHANGED spine
          ELSE /\ UNCHANGED schedulerExpected
               /\ UNCHANGED spine
       /\ UNCHANGED <<invalid, nextIndex, currentSafeBlock, walletNonce,
                      nextL1Slot, l1Included, deadBatches>>

(*
 * SchedulerSkip: advance cursor over a gap (no-op consumed the slot,
 * so no l1Included entry exists).
 *)
SchedulerSkip ==
    /\ schedulerCursor < nextL1Slot
    /\ ~(\E e \in l1Included : e.w_nonce = schedulerCursor)
    /\ schedulerCursor' = schedulerCursor + 1
    /\ UNCHANGED <<spine, invalid, nextIndex, currentSafeBlock,
                   walletNonce, nextL1Slot, l1Included,
                   schedulerExpected, deadBatches>>

---------------------------------------------------------------------------
(*
 * Resolve: the oldest unresolved batch is definitely stale ->
 * cascade-invalidate.
 *
 * Two cases are modeled:
 *   1. the frontier unresolved batch is Silver and stale by inclusion
 *      (the submitted-batch zombie path), or
 *   2. the frontier unresolved batch is Tip and stale by currentSafeBlock
 *      (the aging open-batch path).
 *
 * Cascade-invalidated batches already on L1 (Silver/Bronze) remain
 * in l1Included.  Submitted Pendings become dead batches.
 * Unsubmitted Pendings are discarded.
 *
 * walletNonce is NOT reset — recovery batches use w_nonces past
 * all dead batch slots.
 *
 * The genesis sentinel guarantees fng > 1 (there is always at
 * least one Gold ancestor).
 *)
Resolve ==
    /\ nextIndex <= MaxBatchIndex
    /\ LET fng == FirstNonGold(spine) IN
       /\ fng > 1
       /\ ((spine[fng].color = Silver /\ IsStaleByInclusion(spine[fng]))
           \/ (spine[fng].color = Tip /\ IsStaleByCurrent(spine[fng])))
       /\ LET newLen == fng
              newDead ==
                  {[batch_nonce |-> spine[i].batch_nonce,
                    w_nonce     |-> spine[i].w_nonce,
                    safe_block  |-> spine[i].safe_block] :
                   i \in {j \in fng..Len(spine) :
                          spine[j].color = Pending /\ spine[j].w_nonce # NONE}}
          IN
          /\ spine' = [i \in 1..newLen |->
                          IF i < fng THEN spine[i]
                          ELSE [index          |-> nextIndex,
                                color          |-> Tip,
                                safe_block     |-> currentSafeBlock,
                                inclusion_block |-> 0,
                                w_nonce        |-> NONE,
                                batch_nonce    |-> 0]]
          /\ invalid' = [i \in 1..newLen |->
                            IF i = fng - 1
                            THEN invalid[fng - 1] + (Len(spine) - fng + 1)
                            ELSE IF i < fng THEN invalid[i]
                            ELSE 0]
          /\ nextIndex' = nextIndex + 1
          /\ deadBatches' = deadBatches \union newDead
          /\ UNCHANGED <<currentSafeBlock, walletNonce, nextL1Slot,
                         l1Included, schedulerCursor, schedulerExpected>>

---------------------------------------------------------------------------
(* Spec *)

Next ==
    \/ AdvanceTip
    \/ SubmitBatch
    \/ L1Include
    \/ AdvanceSafeBlock
    \/ SchedulerStep
    \/ SchedulerSkip
    \/ Resolve

Spec == Init /\ [][Next]_vars

=========================================================================
