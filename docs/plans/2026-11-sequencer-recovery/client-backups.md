# Client backups

Recovery asks one thing of a client: that it can restore a state at a count it
trusts. How it keeps its storage is its own business. Two rules make that easy
for a client that keeps its state in SQLite, as Bart's does, at any size:

1. **Every commit is a boundary and keeps its rows.** Apply each feed row, or
   each run of rows, in one SQLite transaction that also writes the L2 tx
   count and era, and stores the rows themselves. Any consistent
   copy of the database, taken at any time, is then at a transaction boundary,
   says which, and holds every row before it.
2. **A new era discards later snapshots.** Inside an era the history only
   appends; only a new era replaces it, past the count the client realigned
   at, and reuses those counts. So once realigned, the client deletes every
   snapshot past that count and stops snapshotting its old live database.

To reach a count, restore the newest snapshot at or below it, replay the rows
after it from the live database or a newer snapshot through the client's
normal apply, and check the count. Never restore a snapshot past the count:
its state holds rows past it. Replay is exact only if applying a row reads
nothing but the database and the row.

Retention sets recovery time, not correctness: a pruned snapshot costs a longer
replay from an older one. Keep recent snapshots and a thinning tail that
outlasts the oldest base machine an operator would pick, and stop pruning once
an incident opens.

## Snapshotting a large SQLite database

From the SQLite, Litestream, OpenZFS and AWS documentation as of 2026-10-29:

- Volume or filesystem snapshots (ZFS, LVM thin, EBS) are the cheap choice at
  terabyte scale: atomic and incremental. A ZFS snapshot holds every completed
  write, so SQLite recovers it to its last committed transaction. An EBS
  snapshot holds only what reached the volume, as a power cut would: with
  `synchronous` NORMAL or FULL that only moves its count back, but with OFF it
  can be corrupt. The database and its `-wal` file must be in the same
  snapshot. Restore by cloning a snapshot, never by rolling back the live
  volume, whose rows the replay reads.
- Litestream suits gigabytes rather than terabytes: by default it uploads a
  full snapshot every 24 hours and keeps 24 hours. It restores only to the end
  of one of its own files (`-txid`, `-timestamp`), which can batch many
  transactions, so pick one at or below the count and let the rows in the live
  database cover the rest. It deletes only by age, so it cannot discard the
  restore points past the realignment count (rule 2): on a new era,
  replicate to a new path, and restore from the old path only at or below
  that count.
- The backup API and `VACUUM INTO` copy every page on every run. On a busy
  database a copy either restarts after each write from another connection or
  holds one read transaction, which stops WAL checkpoints until it ends.
- Every SQLite engine that opens a WAL database needs 3.51.3 or later (or the
  3.44.6 or 3.50.7 backport) once two connections, in threads or processes, can
  write or checkpoint at once: earlier versions can corrupt it. Litestream
  checkpoints with its own embedded engine, fixed from v0.5.17. WAL mode fixes
  the page size, so choose it at creation.
