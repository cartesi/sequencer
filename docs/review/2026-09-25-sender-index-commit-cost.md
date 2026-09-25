# Sender-index cost on the chunk commit — 2026-09-25

Scope: how much `idx_user_ops_sender_nonce`, which serves `GET /nonce`, adds to
the inclusion lane's chunk commit, and how a lane-written per-sender table
compares. Measured on the storage write path committed in `29fa9fb`.

Retained for the register entry on the
[sender index](register.md#known-optimizations). Replace it with a
production-like measurement when that entry is revisited; delete it when the
index decision no longer uses these numbers.

## Environment

- Apple M5 Max (18 cores, 36 GiB), macOS 26.6.2, APFS on the internal SSD.
- Rust 1.95.0 release build, bundled SQLite 3.53.2 (`libsqlite3-sys` 0.38.1).
- The pragmas `Storage::open` sets in production: WAL and `synchronous=FULL`,
  with SQLite's default 1,000-page `wal_autocheckpoint` and 2 MiB page cache.
  On macOS, `FULL` calls `fsync` without `F_FULLFSYNC`, so a commit does not
  wait for the drive cache. That is cheaper than `fsync` on typical cloud block
  storage.

## Method

A throwaway `#[ignore]` test in `storage::ingress::tests`, not committed:

- A fresh database per run, `initialize_open_state`, then 3,000 chunks of 64
  ops (192,000 ops) through `append_executed_user_ops_chunk`, the lane's
  production commit path. The timer wraps only that call.
- One open frame for the whole run: no batch closes, dumps, or frame
  rotations. Ops carry an empty payload and a 65-byte signature. No concurrent
  readers.
- Two sender workloads from a seeded xorshift: every op from a new sender, and
  ops drawn from 1,000 senders.
- Three variants: `index` (the committed schema); `none`
  (`DROP INDEX idx_user_ops_sender_nonce`); and `projection` (index dropped,
  plus a prototype `WITHOUT ROWID` table mapping sender to next nonce, upserted
  per op in the same transaction, without the recovery rewind it would need).
- Each variant ran twice, interleaved. Rounds agreed within about 5%.

## Results

Per 64-op chunk commit, mean / p50 / p99 over 3,000 commits:

| Variant | 1,000 senders | New sender per op |
|---|---|---|
| `none` | 0.27 / 0.26 / 1.1 ms | 0.27 / 0.26 / 1.1 ms |
| `index` | 1.0 / 0.53 / 9.1 ms | 1.0 / 0.54 / 9.3 ms |
| `projection` | 0.32 / 0.30 / 1.1 ms | 0.98 / 0.51 / 8.7 ms |

After 192,000 ops the database file was about 34 MiB without the index, 40 MiB
with it, and 34 or 39 MiB with the projection (1,000 senders or all new).

## Reading

- The cost is WAL volume. Sender-keyed entries land on scattered B-tree leaves,
  so each distinct sender in a chunk dirties about one more 4 KiB page. The WAL
  then reaches the autocheckpoint threshold several times sooner, and the
  checkpoint runs inline in the lane's `COMMIT`, which is the p99 column.
- The index pays this at any population size because it grows with total ops.
  A per-sender table pays it once the sender population outgrows a few pages:
  roughly 9,000 senders for 64-op chunks at about 30 bytes per row. Below that
  it stays hot and cheap.
- In absolute terms the index adds about 12 µs per included op on average and
  up to about 8 ms at p99 per commit, against the 500 ms ack target.

## Limits

- One machine with a cheap `fsync`. Where `fsync` dominates the commit, the
  relative overhead is likely smaller; the extra WAL bytes remain.
- No payloads, batch closes, dumps, readers, or application execution. A full
  lane turn costs more than the commit alone, so the index is a smaller share
  of it.
- The projection figures omit the rewind it would need on recovery.
