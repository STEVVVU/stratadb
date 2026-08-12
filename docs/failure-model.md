# Failure Model

## Guarantees

- A successful write returns only after the full batch record is appended and
  `sync_data` succeeds.
- Recovery exposes all or none of each checksummed batch.
- The index never points to a write that has not completed synchronization.
- A stale or partial active tail is truncated to the last valid record.
- Corruption in a sealed segment fails open.
- Compaction keeps old segments until every replacement segment is visible.
- One cooperating process can hold the database writer lock at a time.

## Write crash points

| Crash point | Recovery result |
| --- | --- |
| Before append | Previous state |
| During header or payload append | Partial final record is truncated |
| After append, before/during synchronization | Record is accepted only if fully present and checksummed |
| After synchronization, before index update | Replay publishes the complete batch |
| After index update | Complete batch remains visible |

The in-memory index is disposable. No checkpoint is required for correctness.

## Rotation crash points

If a crash occurs before the next segment is created, the old segment remains
active. If it occurs after creation but before append, recovery sees an empty
highest-ID segment. The previous segment is valid and replayed strictly.

## Compaction crash points

Temporary files are not replayed. A crash while writing them leaves old
segments authoritative. Once a compacted file is renamed, its higher segment
ID makes its values override old copies during replay. Old segments are
deleted only after all output renames and the in-memory switch.

Deleting superseded files is idempotent with respect to data contents: either
old and new records coexist, or only the complete new records remain.

## Corruption policy

The final segment's first invalid record defines the repair boundary. Repair
discards that record and every later byte in the segment. This is appropriate
for append interruption and torn tails, but an arbitrary bit flip in an older
record of the active segment can also discard valid records after it.

Earlier segments are immutable and strict. Any malformed header, impossible
length, invalid operation, sequence regression, or CRC mismatch returns a
corruption error. The operator must restore or replace that segment; StrataDB
does not guess at record resynchronization.

## Process crash versus power loss

The tested guarantee is process-crash recovery on a filesystem honoring
successful file synchronization and atomic same-directory rename. WAL record
data and compaction outputs are synchronized.

Version 0.1 does not explicitly fsync the parent directory after segment
creation, rename, or deletion on every platform. Consequently, sudden power
loss can expose filesystem-specific metadata outcomes even when process-crash
tests pass. A production hardening step is a manifest protocol plus portable
directory synchronization.

## Excluded failures

- Hardware or filesystem behavior that violates successful sync/rename
  contracts
- Simultaneous writers that bypass advisory locking
- Memory exhaustion while rebuilding the full index or snapshot
- External application side effects
- Replication, network partitions, and multi-host failover
