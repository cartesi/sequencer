---------------------------- MODULE optimistic -----------------------------
(*
 * Formal model of sequencer batch tree with scheduler, wallet nonces,
 * zombie batches, and adversarial L1 inclusion.
 *
 * Proves: ZombieSafety == schedulerExpected = CountGold(spine)
 *
 * After recovery, no zombie batch from an invalidated chain is ever
 * accepted by the scheduler.
 *
 * Colors (spine ordering): Gold* Silver* Bronze* Pending* Tip
 *   - Tip:     open batch (not yet closed)
 *   - Pending: closed, may have w_nonce (submitted to L1 mempool)
 *   - Bronze:  included in an L1 block (not yet safe)
 *   - Silver:  included in a safe L1 block
 *   - Gold:    accepted by the scheduler
 *
 * Key mechanism — two-layer zombie protection:
 *   (1) Wallet nonce mutual exclusion: zombie and recovery batch compete
 *       for the same L1 slot. Loser's w_nonce is bumped.
 *   (2) Nonce poisoning: stale batch is a no-op in the scheduler (does
 *       not increment expected nonce), making all subsequent zombies
 *       have wrong batch_nonce.
 *
 * Actions:
 *   AdvanceTip       -- close tip -> Pending, append new Tip
 *   SubmitBatch      -- assign w_nonce to first unsubmitted Pending
 *   L1Include        -- include tx at nextL1Slot (spine or zombie wins)
 *   AdvanceSafeBlock -- L1 safe block advances, Bronze -> Silver
 *   SchedulerStep    -- scheduler processes next safe L1 entry + Gold
 *   Resolve          -- detect staleness, cascade, create zombies
 *
 * See docs/recovery.md for the conceptual model.
 *)

EXTENDS Integers, Sequences, FiniteSets

CONSTANTS
    MaxBatchIndex,      \* bound on total batch creations
    MaxSafeBlock,       \* bound on L1 safe block
    MAX_WAIT_BLOCKS     \* staleness threshold

NONE == -1              \* sentinel: "no w_nonce assigned"

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
    spine,              \* Seq of [index, color, safe_block, inclusion_block,
                        \*         w_nonce, batch_nonce]
    invalid,            \* Seq of Nat: dead-branch count per spine position
    nextIndex,          \* Nat: next batch index
    currentSafeBlock,   \* Nat: L1 safe block (environment)
    walletNonce,        \* Nat: next w_nonce for mempool submission
    zombies,            \* Set of [batch_nonce, w_nonce, safe_block]
    nextL1Slot,         \* Nat: L1 nonce cursor (next w_nonce to include)
    l1Included,         \* Set of [batch_nonce, w_nonce, inclusion_block,
                        \*         safe_block, is_safe]
    schedulerCursor,    \* Nat: next w_nonce the scheduler will process
    schedulerExpected   \* Nat: scheduler's expected batch nonce

vars == <<spine, invalid, nextIndex, currentSafeBlock,
          walletNonce, zombies, nextL1Slot, l1Included,
          schedulerCursor, schedulerExpected>>

---------------------------------------------------------------------------
(* Helpers *)

CountGold(s) == Cardinality({i \in 1..Len(s) : s[i].color = Gold})

FirstNonGold(s) ==
    IF \E i \in 1..Len(s) : s[i].color # Gold
    THEN CHOOSE i \in 1..Len(s) :
            s[i].color # Gold /\ \A j \in 1..i-1 : s[j].color = Gold
    ELSE 0

\* First Pending without a w_nonce.
FirstUnsubmitted(s) ==
    IF \E i \in 1..Len(s) : s[i].color = Pending /\ s[i].w_nonce = NONE
    THEN CHOOSE i \in 1..Len(s) :
            s[i].color = Pending /\ s[i].w_nonce = NONE
            /\ \A j \in 1..i-1 : ~(s[j].color = Pending /\ s[j].w_nonce = NONE)
    ELSE 0

\* Spine position of Pending batch with a given w_nonce.
PendingAtWNonce(s, wn) ==
    IF \E i \in 1..Len(s) : s[i].color = Pending /\ s[i].w_nonce = wn
    THEN CHOOSE i \in 1..Len(s) : s[i].color = Pending /\ s[i].w_nonce = wn
    ELSE 0

\* Spine position of Silver batch with a given batch_nonce.
SilverAtBN(s, bn) ==
    IF \E i \in 1..Len(s) : s[i].color = Silver /\ s[i].batch_nonce = bn
    THEN CHOOSE i \in 1..Len(s) : s[i].color = Silver /\ s[i].batch_nonce = bn
    ELSE 0

---------------------------------------------------------------------------
(* Staleness *)

IsStaleByInclusion(b) == b.inclusion_block - b.safe_block >= MAX_WAIT_BLOCKS
IsStaleByCurrentBlock(b) == currentSafeBlock - b.safe_block >= MAX_WAIT_BLOCKS

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

\* Gold* Silver* Bronze* Pending* Tip
SpineOrdering ==
    /\ spine[Len(spine)].color = Tip
    /\ \A i \in 1..Len(spine)-1 :
        ColorOrd(spine[i].color) <= ColorOrd(spine[i+1].color)

SafeBlockMonotonic ==
    \A i \in 1..Len(spine)-1 :
        (spine[i].color # Tip /\ spine[i+1].color # Tip)
        => spine[i].safe_block <= spine[i+1].safe_block

InvalidOnlyOnGold ==
    \A i \in 1..Len(spine) : invalid[i] > 0 => spine[i].color = Gold

CurrentStalenessMonotonic ==
    \A i, j \in 1..Len(spine) :
        (i < j /\ spine[i].color # Tip /\ spine[j].color # Tip
         /\ IsStaleByCurrentBlock(spine[j]))
        => IsStaleByCurrentBlock(spine[i])

BatchNoncesContiguous ==
    \A i \in 1..Len(spine) :
        spine[i].color # Tip => spine[i].batch_nonce = i - 1

\* ------- THE KEY THEOREM -------
ZombieSafety == schedulerExpected = CountGold(spine)

\* Supporting L1 invariants
L1WNonceUnique ==
    \A e1, e2 \in l1Included : e1.w_nonce = e2.w_nonce => e1 = e2

ZombieNotYetIncluded ==
    \A z \in zombies : z.w_nonce >= nextL1Slot

L1BeforeCursor ==
    \A e \in l1Included : e.w_nonce < nextL1Slot

SchedulerBehindL1 ==
    schedulerCursor <= nextL1Slot

Inv ==
    /\ TypeOK
    /\ SpineOrdering
    /\ SafeBlockMonotonic
    /\ InvalidOnlyOnGold
    /\ CurrentStalenessMonotonic
    /\ BatchNoncesContiguous
    /\ ZombieSafety
    /\ L1WNonceUnique
    /\ ZombieNotYetIncluded
    /\ L1BeforeCursor
    /\ SchedulerBehindL1

---------------------------------------------------------------------------
(* Initial state *)

Init ==
    /\ spine = <<[index |-> 0, color |-> Tip, safe_block |-> 0,
                  inclusion_block |-> 0, w_nonce |-> NONE, batch_nonce |-> 0]>>
    /\ invalid = <<0>>
    /\ nextIndex = 1
    /\ currentSafeBlock = 0
    /\ walletNonce = 0
    /\ zombies = {}
    /\ nextL1Slot = 0
    /\ l1Included = {}
    /\ schedulerCursor = 0
    /\ schedulerExpected = 0

---------------------------------------------------------------------------
(*
 * AdvanceTip: close the current Tip -> Pending, append new Tip.
 * Assigns safe_block (from environment) and batch_nonce.
 *)
AdvanceTip ==
    /\ nextIndex <= MaxBatchIndex
    /\ LET tipPos == Len(spine)
       IN
       /\ spine[tipPos].color = Tip
       /\ \E sb \in 0..currentSafeBlock :
            /\ (tipPos > 1 => sb >= spine[tipPos - 1].safe_block)
            /\ spine' = [i \in 1..Len(spine) + 1 |->
                IF i < tipPos THEN spine[i]
                ELSE IF i = tipPos
                     THEN [index          |-> spine[tipPos].index,
                           color          |-> Pending,
                           safe_block     |-> sb,
                           inclusion_block |-> 0,
                           w_nonce        |-> NONE,
                           batch_nonce    |-> tipPos - 1]
                     ELSE [index          |-> nextIndex,
                           color          |-> Tip,
                           safe_block     |-> 0,
                           inclusion_block |-> 0,
                           w_nonce        |-> NONE,
                           batch_nonce    |-> 0]]
            /\ invalid' = [i \in 1..Len(spine) + 1 |->
                              IF i <= Len(spine) THEN invalid[i] ELSE 0]
            /\ nextIndex' = nextIndex + 1
       /\ UNCHANGED <<currentSafeBlock, walletNonce, zombies,
                      nextL1Slot, l1Included, schedulerCursor,
                      schedulerExpected>>

---------------------------------------------------------------------------
(*
 * SubmitBatch: assign w_nonces to ALL unsubmitted Pending batches
 * at once, in spine-position order.  This models the real batch
 * submitter which reads the on-chain nonce and submits every
 * pending batch each tick.
 *)
SubmitBatch ==
    LET unsubPos == {i \in 1..Len(spine) :
                        spine[i].color = Pending /\ spine[i].w_nonce = NONE}
        \* Read on-chain nonce: can't use a slot L1 already consumed
        wn0 == IF walletNonce >= nextL1Slot THEN walletNonce ELSE nextL1Slot
    IN
    /\ unsubPos # {}
    /\ spine' = [i \in 1..Len(spine) |->
                  IF i \in unsubPos
                  THEN [spine[i] EXCEPT
                          !.w_nonce = wn0 + Cardinality({j \in unsubPos : j < i})]
                  ELSE spine[i]]
    /\ walletNonce' = wn0 + Cardinality(unsubPos)
    /\ UNCHANGED <<invalid, nextIndex, currentSafeBlock, zombies,
                   nextL1Slot, l1Included, schedulerCursor,
                   schedulerExpected>>

---------------------------------------------------------------------------
(*
 * L1Include: include one transaction at w_nonce = nextL1Slot.
 *
 * If both a spine Pending and a zombie exist at this slot, L1
 * non-deterministically picks one (mempool competition).
 *
 * Spine wins: Pending -> Bronze (or Silver if block already safe).
 * Zombie wins: zombie included; competing Pending's w_nonce bumped.
 *
 * inclusion_block >= currentSafeBlock (L1 monotonicity: transactions
 * are included in current or future blocks) and >= all previous
 * inclusion blocks (block numbers are monotonic).
 *)

L1IncludeSpine ==
    LET pos == PendingAtWNonce(spine, nextL1Slot)
    IN
    /\ pos > 0
    /\ \E ib \in currentSafeBlock..MaxSafeBlock :
        \* Block ordering: non-decreasing inclusion_block
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
           \* Kill zombie at this slot if it existed
           /\ zombies' = {z \in zombies : z.w_nonce # nextL1Slot}
           /\ UNCHANGED <<invalid, nextIndex, currentSafeBlock,
                          walletNonce, schedulerCursor, schedulerExpected>>

L1IncludeZombie ==
    /\ \E z \in zombies : z.w_nonce = nextL1Slot
    /\ LET z == CHOOSE zz \in zombies : zz.w_nonce = nextL1Slot
       IN
       \E ib \in currentSafeBlock..MaxSafeBlock :
        /\ \A e \in l1Included : ib >= e.inclusion_block
        /\ l1Included' = l1Included \union
              {[batch_nonce    |-> z.batch_nonce,
                w_nonce        |-> nextL1Slot,
                inclusion_block |-> ib,
                safe_block     |-> z.safe_block,
                is_safe        |-> (ib <= currentSafeBlock)]}
        /\ nextL1Slot' = nextL1Slot + 1
        /\ zombies' = {zz \in zombies : zz.w_nonce # nextL1Slot}
        \* If a spine Pending was competing at this slot, reset ALL
        \* submitted Pending w_nonces.  The batch submitter will
        \* re-read the on-chain nonce and resubmit everything.
        /\ LET hasConflict == PendingAtWNonce(spine, nextL1Slot) > 0
           IN
           IF hasConflict
           THEN /\ spine' = [i \in 1..Len(spine) |->
                              IF spine[i].color = Pending
                                 /\ spine[i].w_nonce # NONE
                              THEN [spine[i] EXCEPT !.w_nonce = NONE]
                              ELSE spine[i]]
                /\ walletNonce' = nextL1Slot + 1
           ELSE /\ UNCHANGED spine
                /\ UNCHANGED walletNonce
        /\ UNCHANGED <<invalid, nextIndex, currentSafeBlock,
                       schedulerCursor, schedulerExpected>>

L1Include == L1IncludeSpine \/ L1IncludeZombie

---------------------------------------------------------------------------
(*
 * AdvanceSafeBlock: environment advances the L1 safe block.
 * Bronze -> Silver on spine when inclusion_block becomes safe.
 * Marks l1Included entries as safe.
 *)
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
        /\ UNCHANGED <<invalid, nextIndex, walletNonce, zombies,
                       nextL1Slot, schedulerCursor, schedulerExpected>>

---------------------------------------------------------------------------
(*
 * SchedulerStep: process the L1 entry at schedulerCursor.
 *
 * The on-chain scheduler sees L1 inputs in w_nonce order and
 * maintains an expected batch nonce counter.
 *
 * Accept: batch_nonce matches AND not stale by inclusion.
 *   -> increment schedulerExpected, promote spine Silver -> Gold.
 * Skip: nonce mismatch OR stale (nonce poisoning).
 *   -> schedulerExpected unchanged.
 *
 * If accepted but the batch is not on the spine (zombie was accepted),
 * spine is unchanged but schedulerExpected increments. ZombieSafety
 * would then be violated — which is exactly what we're proving
 * cannot happen.
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
                      zombies, nextL1Slot, l1Included>>

---------------------------------------------------------------------------
(*
 * Resolve: detect staleness at the frontier, cascade-invalidate,
 * create zombies from submitted Pending batches, open recovery Tip.
 *
 * CRITICAL: the frontier must be Silver (safe on L1) before we
 * cascade.  This guarantees the stale batch is permanently on L1
 * and the scheduler WILL see it and be poisoned — no mempool
 * mutual exclusion can kill it.  Detecting staleness on Bronze
 * or Pending would allow a race where the recovery batch takes
 * the frontier's L1 slot, preventing nonce poisoning and letting
 * non-frontier zombies be accepted (see counterexample in commit
 * history).
 *
 * Only submitted Pending batches (w_nonce # NONE) become zombies.
 * Bronze/Silver batches are already in l1Included; the scheduler
 * will process and reject them (stale or nonce mismatch).
 *
 * walletNonce is reset to nextL1Slot: the sequencer reads the
 * latest on-chain nonce and resubmits from there.
 *)
Resolve ==
    /\ nextIndex <= MaxBatchIndex
    /\ LET fng == FirstNonGold(spine)
       IN
       /\ fng > 0
       /\ fng > 1                    \* need a Gold parent
       /\ spine[fng].color = Silver  \* ONLY Silver — must be safe on L1
       /\ IsStaleByInclusion(spine[fng])
       /\ LET newLen == fng           \* (fng-1) Golds + 1 new Tip
              \* Zombies from submitted Pending batches in the cascade
              newZombies ==
                  {[batch_nonce |-> spine[i].batch_nonce,
                    w_nonce     |-> spine[i].w_nonce,
                    safe_block  |-> spine[i].safe_block] :
                   i \in {j \in fng..Len(spine) :
                          spine[j].color = Pending /\ spine[j].w_nonce # NONE}}
          IN
          /\ spine' = [i \in 1..newLen |->
                          IF i < fng THEN spine[i]  \* all Gold
                          ELSE [index          |-> nextIndex,
                                color          |-> Tip,
                                safe_block     |-> 0,
                                inclusion_block |-> 0,
                                w_nonce        |-> NONE,
                                batch_nonce    |-> 0]]
          /\ invalid' = [i \in 1..newLen |->
                            IF i = fng - 1
                            THEN invalid[fng - 1] + (Len(spine) - fng + 1)
                            ELSE IF i < fng THEN invalid[i]
                            ELSE 0]
          /\ nextIndex' = nextIndex + 1
          /\ zombies' = zombies \union newZombies
          /\ walletNonce' = nextL1Slot
          /\ UNCHANGED <<currentSafeBlock, nextL1Slot, l1Included,
                         schedulerCursor, schedulerExpected>>

---------------------------------------------------------------------------
(* Spec *)

Next ==
    \/ AdvanceTip
    \/ SubmitBatch
    \/ L1Include
    \/ AdvanceSafeBlock
    \/ SchedulerStep
    \/ Resolve

Spec == Init /\ [][Next]_vars

=========================================================================
