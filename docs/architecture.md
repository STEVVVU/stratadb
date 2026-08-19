# Architecture

StrataDB is an embedded storage library, not a server. All state belongs to one
`Engine` instance and one database directory.

```text
 application threads
       |             write batch
       |        +-------------------+
       |        | writer mutex      |
       |        | encode -> append  |
       |        | -> sync -> index  |
       |        +---------+---------+
       |                  |
       v                  v
+--------------+    +-----------+        +-------------------+
| state RwLock |    | active WAL| -----> | rotated segments  |
| ordered index|    +-----------+        +-------------------+
| segment fds  |          ^                       |
+------+-------+          +------ compaction -----+
       |
       +---- cached read-only handles + positional reads
```

## Ownership and concurrency

Opening a database takes an exclusive OS lock on `.stratadb.lock`. This
prevents a second process or `Engine` instance from writing the directory.
The lock is released when the process or file handle exits. Tools that bypass
`Engine`, such as strict `inspect`, are documented as offline operations.

Within one engine:

- A writer mutex serializes WAL appends, rotation, and compaction.
- An `RwLock` protects the ordered index, sequence, and segment-handle set.
- Point reads hold a shared lock while issuing positional I/O. They do not
  mutate a shared file cursor.
- A write takes the writer mutex and then the state write lock. Compaction uses
  the same order, preventing lock-order cycles.

## Write path

1. Validate operation count, key length, value length, and encoded record size.
2. Allocate the next logical sequence while holding the write path.
3. Encode the entire batch as one checksummed WAL record.
4. Rotate first if the record would cross the configured segment target.
5. Append the record and call `sync_data`.
6. Publish each key's segment/value offset or deletion to the index.

The index is never ahead of durable WAL. A crash after synchronization but
before index publication is repaired by replay on the next open.

## Read path and snapshots

The index maps each live key to `(segment_id, value_offset, value_length,
sequence)`. Segment files have cached read-only handles. `get` uses `read_at`
on Unix or `seek_read` on Windows, allowing concurrent readers without a
shared seek position.

Snapshots hold the state read lock while materializing every live key/value
pair into an immutable `Arc<BTreeMap<...>>`. This is intentionally simple and
strong: later writes, rotation, and compaction cannot change an existing
snapshot. Its cost is proportional to live data size.

## Startup recovery

Segment names contain zero-padded increasing IDs. Recovery scans them in ID
order, checks record structure and CRC, verifies non-decreasing sequences, and
rebuilds the index from value offsets.

Every segment except the highest ID is sealed and scanned strictly. The final
segment permits suffix repair: bytes from the first partial or invalid record
to EOF are discarded and synchronized. This handles interrupted append and
torn record tails while avoiding silent loss from old segments.

## Rotation and compaction

Rotation synchronizes the old segment, creates and synchronizes a higher-ID
segment, then directs future appends there. A single record may exceed the
target segment size but can never exceed the hard record limit.

Compaction:

1. Reads the current live index under the writer/state locks.
2. Writes bounded put-only records to higher-ID `.wal.tmp` files.
3. Synchronizes each temporary file and atomically renames it to `.wal`.
4. Opens new read handles and switches the writer/index to compacted segments.
5. Removes superseded segments.

Old segments remain authoritative until all compacted outputs are visible.
If a process exits during rename, visible higher-ID records safely override
old records during replay; leftover temporary files are ignored and removed
on the next locked open.

## Scaling boundary

The design favors inspectability and deterministic local verification. Writes
are serialized, and SQLite-style page management is intentionally absent.
Future growth points are a block cache, sparse/multi-level index, range
iterators, background compaction, and a manifest with explicit directory-sync
protocols.

Two of these are load-bearing rather than nice-to-have, and benchmarking against
redb and sled quantified both:

- **No block cache and no zero-copy read.** Every `get` costs a positional-read
  syscall plus a fresh allocation. Measured at roughly 11x slower than sled on
  random point reads. The index itself is not the bottleneck: a miss, which stops
  after the `BTreeMap` lookup, is about 28x faster than a hit.
- **No checkpoint or on-disk index.** Startup is O(bytes on disk) because the
  entire WAL is replayed and every CRC re-verified. Measured at roughly 50x
  slower than redb to open a 50,000-key database, and the ratio grows with size.

These are the price of the write path above: appends stay cheap precisely because
index construction is deferred to startup and no page structure is maintained on
disk. Memory mapping was measured as a fix for the first and rejected; see the
failure model for why truncation-based tail repair forecloses it.
