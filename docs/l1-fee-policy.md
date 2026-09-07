# L1 fee policy

The batch poster uses a fresh market estimate on every tick, with no fee
floor carried from earlier attempts. This keeps retry frequency from
compounding the prices offered for pending batches. It accepts possible
prolonged submission stalls; prompt inclusion is not guaranteed.

## Submission

Each tick derives the unresolved batch suffix from persisted and observed
L1 state and attempts every payload, including those behind a refused
replacement. A remembered transaction hash never causes a payload to be
skipped. Gas is estimated without a nonce, at Latest, and padded before the
wallet nonce is attached to the send.

The [estimator](../sequencer/src/l1/eip1559.rs) uses ten historical blocks,
the median positive 20th-percentile reward as the priority cap, and
`fee cap = 2 × base fee + priority cap`. Geth's default replacement policy
requires both caps to increase strictly and meet its 10% bump threshold
(with integer rounding). A fresh estimate can improve one component without
clearing the other.

`already known` and `replacement transaction underpriced` can occur while
the original is still adequately priced. They do not by themselves establish
an inclusion problem. Recognized mempool conflicts and confirmation timeouts
produce `Waiting`; the worker sleeps its idle interval before re-estimating.
Other provider errors retain their existing error path.

## Accepted limits

A pending transaction can become unmineable when its fee cap falls below the
base fee, while fresh estimates still fail the replacement rule. It can also
remain mineable but uncompetitive. Full blocks do not guarantee a rising tip
estimate: base-fee changes are protocol rules, while tips reflect the fee
market and builder choices. Clearing within minutes is an operational
expectation to measure, not a bound this policy establishes.

The danger detector stops normal operation when the configured danger
threshold is reached. That bounds continued soft-confirmation issuance under
the detector's assumptions; it does not bound inclusion or recovery duration.
[Recovery](recovery/README.md#step-4-post-flush-state) requires all covered
wallet slots to resolve at safe depth before a cascade can proceed. The
flusher's fixed headroom can also fail to replace an unmineable original.
The sequencer remains offline until recovery succeeds or the operator acts.

The poster warns after a nonce has been unresolved for five minutes and at
most once per further interval. Accepted re-broadcasts do not reset its age.
The clock starts at this process's first attempt and resets on restart, so it
is a lower bound on unresolved age. A sequencer restart does not itself clear
the Ethereum node's pending transactions.

## Why this policy

The [#34 review history](https://github.com/cartesi/sequencer/pull/34) explored
asymmetric bumps, remembered suffix hashes, compounding fee floors, and a
market-relative ceiling. Those implementations introduced invalid fee pairs,
payload/hash-association bugs, funding pressure, and recovery incompatibility.
[#35](https://github.com/cartesi/sequencer/pull/35) retains the independent
gas-estimation and flusher fee-validity fixes while removing that escalation
state. These failures motivate the current choice; they do not prove that
every bounded replacement policy would be unsound.

Revisit with evidence of sustained batch delays, recovery frequency or
duration, and submission cost. Record the logs and affected L1 block range.
Any future escalation policy needs an explicit urgency trigger, spending
budget, and compatible recovery pricing. A rejected replacement alone is
insufficient motivation.

Fee-policy tests must distinguish [geth's two-component replacement
check](https://github.com/ethereum/go-ethereum/blob/master/core/txpool/legacypool/list.go)
from [Anvil's gas-price replacement
check](https://github.com/foundry-rs/foundry/blob/v1.4.3/crates/anvil/src/eth/pool/transactions.rs).
Anvil exercises the send and retry flow but does not establish geth's fee
acceptance behavior.
