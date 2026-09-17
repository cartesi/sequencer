# Review notes and their lifecycle

Reviews help us investigate code and hand work over. Their conclusions must
survive where future contributors need them; their working notes need only
survive while useful. Git is the archive.

## During a review

Create a note only when the investigation or handoff needs one. A small review
can live in the conversation, PR, or commit description. Commit a note when
sharing the ongoing reasoning is useful; committing it does not make it a
permanent document.

Notes are mutable. Correct, consolidate, and remove superseded claims instead
of appending a conversation transcript. Record the reviewed revision, scope,
evidence, uncertainty, and next question. Check behavior in code and tests;
documentation and earlier verdicts are leads, not proof. Separate observed
defects from coverage gaps, accepted tradeoffs, and unverified hypotheses.

## Closing or handing off

Give each surviving conclusion one home:

| Conclusion | Home |
|---|---|
| Current contract, invariant, or durable design reason | Its owning design document, source comment, or test |
| Unresolved defect or investigation | [Current register](register.md), unless already owned by an active plan |
| Coordinated implementation or integration work | The relevant active plan; link to it from the register if useful |
| Measurement or other evidence still used by a decision | A dated record with the consumer, exact revision, method, result, limits, and retirement condition |
| Completed discussion, superseded proposal, closed finding | Git history; remove it from the working tree |

An unresolved entry states the impact or question, supporting source/test,
last-checked date and revision, and next action or revisit condition. A shared
verification stamp is sufficient for entries checked together. A missing test
needs a specific unverified behavior; an absent test name alone creates no
obligation. Proposed machinery needs an invariant and supported assumptions.

Preserve the reason for a deliberate tradeoff at its design seam, together with
the assumptions that would change the decision. A previous rejection is not a
permanent ban on an alternative. Avoid a second register of settled/refuted
decisions, closed-item tombstones, or review-codename maps.

Delete the finished note after checking that useful unresolved work and unique
evidence have a home, and update incoming links. Apply the same rule to
completed plans. Revisit retained notes when related code changes, at handoff,
or when they stop informing a decision; no calendar-driven archive is needed.
Cleanup does not imply the remaining code work is complete.

## Recovering earlier reasoning

The last version of a deleted note remains available without loading it into
every agent's baseline context:

```sh
git log --all -- docs/review/FILE.md
git show REMOVAL_COMMIT^:docs/review/FILE.md
```

Treat that version as evidence about its reviewed revision. Recheck its
premises before carrying a conclusion into current work.
