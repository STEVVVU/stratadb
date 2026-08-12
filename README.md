# StrataDB

StrataDB is a crash-safe embedded key-value storage engine written in Rust. It
uses a checksummed append-only write-ahead log, an in-memory key-to-file-offset
index, atomic write batches, immutable snapshots, segment rotation, and
copy-forward compaction.

The project has no third-party runtime dependencies and does not require
Docker. It targets Rust 1.89 or newer.

## Storage behavior

- Binary `get`, `put`, and `delete` operations with explicit key/value limits
- Atomic multi-operation batches represented by one WAL record
- CRC-32 checksums over record metadata and payload
- Startup recovery that truncates torn or corrupt data from the active tail
- Strict failure on corruption in sealed segments
- Monotonic sequence numbers and point-in-time, materialized snapshots
- Configurable segment rotation and crash-safe copy-forward compaction
- One cross-process writer enforced by an OS file lock
- Concurrent in-process readers using cached handles and positional I/O
- Offline WAL inspection plus explicit repair and benchmark commands

See [the architecture](docs/architecture.md),
[on-disk format](docs/on-disk-format.md), and
[failure model](docs/failure-model.md) for the precise guarantees.

## Build and test

```powershell
cargo build --release
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

The API is usable directly:

```rust
use stratadb::{Engine, WriteBatch};

let db = Engine::open("./example-db")?;
db.put(b"user:42", b"active")?;

let mut batch = WriteBatch::new();
batch
    .put(b"balance:42".to_vec(), b"125".to_vec())
    .delete(b"pending:42".to_vec());
db.write(&batch)?;

let snapshot = db.snapshot()?;
assert_eq!(snapshot.get(b"balance:42"), Some(b"125".as_slice()));
# Ok::<(), stratadb::Error>(())
```

## CLI operations

```powershell
cargo run --release -- put .tmp/demo account:42 active
cargo run --release -- get .tmp/demo account:42
cargo run --release -- stats .tmp/demo
cargo run --release -- inspect .tmp/demo
cargo run --release -- compact .tmp/demo
```

`inspect` performs a strict, read-only record scan and reports segment offsets,
sequences, and operation counts. It should be run while the database is
offline because it intentionally does not acquire the writer lock.

`repair` acquires the writer lock, verifies all sealed segments, and truncates
only the invalid suffix of the final active segment:

```powershell
cargo run --release -- repair .tmp/demo
```

## Recovery model

A write batch is encoded completely, appended, and synchronized with
`sync_data` before the in-memory index changes. Recovery applies only complete,
validly checksummed records. Therefore a process crash can expose either none
or all of a batch, never a prefix of its operations.

The final segment is the only segment eligible for automatic tail repair.
Invalid data in an earlier sealed segment fails open rather than silently
discarding later committed history. External side effects are outside the
engine's transaction boundary.

## Reproducible benchmark

Run the fixed seeded workload into a new or empty directory:

```powershell
cargo run --release -- bench .tmp/bench 10000 256 42
```

The workload performs 10,000 inserts in 100-operation durable batches, 10,000
seeded point reads, a full snapshot, compaction, close/reopen recovery, and
key-count validation. It prints machine-readable JSON and refuses to overwrite
a non-empty benchmark directory.

One local run on July 16, 2026 used Windows x86-64, Rust 1.96.0, 256-byte
values, batch size 100, and seed 42:

| Phase | Measured result |
| --- | ---: |
| Durable batched writes | 18,397.57 operations/s |
| Indexed positional point reads | 117,113.30 reads/s |
| Materialize 10,000-key snapshot | 0.054160 s |
| Copy-forward compaction | 0.175159 s |
| Reopen and rebuild index | 0.059541 s |
| Keys verified after reopen | 10,000 / 10,000 |

The exact output is checked in at
[`benchmarks/windows-x86_64-2026-07-16.json`](benchmarks/windows-x86_64-2026-07-16.json).
This is a single development-machine observation, not a production capacity
claim. Results depend heavily on filesystem, storage cache, sync semantics,
batch size, and value distribution.

## Limits

- Storage is single-writer and single-host; there is no replication,
  multi-process read-only mode, or distributed consensus.
- The index stores every key and value location in memory. Snapshots copy every
  live value into memory.
- `get` allocates a new value buffer. There is no block cache, compression,
  range scan, bloom filter, or memory-mapped I/O.
- Automatic repair assumes the first invalid record in the active segment
  begins an unusable tail. Sealed-segment corruption requires restore from a
  known-good copy.
- Record data is synchronized, but parent-directory metadata is not explicitly
  fsynced after create/rename on every platform. See the failure model for the
  distinction between process-crash and sudden-power-loss guarantees.

## Resume bullets

- Built a dependency-free Rust embedded key-value engine with a versioned
  CRC-32 WAL, bounded binary records, atomic batches, indexed positional reads,
  sequence-consistent snapshots, and OS-enforced single-writer locking.
- Implemented deterministic crash recovery and segment lifecycle management:
  active-tail truncation, sealed-segment corruption rejection, segment
  rotation, and copy-forward compaction that preserves recoverability across
  process interruption.
- Added byte-level fault injection, eight-reader concurrency tests, offline
  inspection/repair tooling, and a seeded benchmark measuring 18.4K durable
  writes/s and 117K point reads/s for 10,000 256-byte values on local Windows.

## License

MIT
