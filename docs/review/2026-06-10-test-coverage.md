# Test-coverage review (2026-06-10)

Third companion to the correctness review: what the suite pins, what it
fails to pin, and which harness levers exist or are missing. **Verdict:**
recovery is the best-tested subsystem (the full dispatch matrix at unit and
e2e level, libfaketime mid-run clock jumps, respawn loops, TCP-proxy outage
injection, Anvil mempool control); the one structural hole was the duality
having no direct mechanism — closed since by the watchdog non-genesis
byte-compare e2e (sequencer-produced batches through the real guest
machine) and the I1 predicate-vs-fold agreement table.

The owed-test list, remaining harness levers, and accepted-untested items
live in [`register.md`](register.md) ("Owed tests"), with statuses verified
2026-08-22. Decision recorded there: do not resurrect a TEST_PLAN scenario
matrix — the dated, finite owed list is the artifact.
