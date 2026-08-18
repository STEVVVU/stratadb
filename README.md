# StrataDB

StrataDB is a crash-safe embedded key-value storage engine written in Rust. It
uses a checksummed append-only write-ahead log, an in-memory key-to-file-offset
index, atomic write batches, immutable snapshots, segment rotation, and
copy-forward compaction.

It targets Rust 1.89 or newer and does not require Docker.

## Dependencies and the from-scratch boundary

**The storage engine is written from scratch.** The WAL record format, CRC
domain, recovery and tail-repair logic, the key→offset index, segment rotation,
and copy-forward compaction are all implemented in this repository. No
dependency supplies any part of that, and this is deliberate: the engine is the
point of the project.

Third-party crates are used where a hand-rolled version would be strictly worse
and carries no design opinion:

| Crate | Version | Why |
| --- | --- | --- |
| `crc32fast` | 1.5 | Hardware-accelerated CRC-32/ISO-HDLC (PCLMULQDQ). Measured **~95x faster at 256 B and ~190x at 64 KiB** than the previous scalar implementation. Same polynomial, same output. A real engine would not ship a bitwise CRC. |
| `thiserror` | 2.0 | Derives `Display`/`Error` for the error enum. Message text is pinned by a test, so nothing user-visible changed. |
| `criterion` (dev) | 0.7 | Statistical benchmarking with confidence intervals and outlier detection. |
| `tempfile` (dev) | 3.27 | Scratch directories for tests and benchmarks, cleaned up even on panic. |
| `redb`, `sled` (optional) | 2.6, 0.34 | Comparison benchmarks only, behind `--features compare`. Not linked into the library or the CLI. |

The boundary is: **the engine is from-scratch; the checksum primitive and the
benchmarking/testing harness come from the ecosystem.** The default build pulls
exactly two runtime crates (`crc32fast`, `thiserror`); `--features compare` is
the only thing that pulls in another storage engine.

`serde` was considered and rejected. The only serialization boundary is the CLI
`bench` JSON report — a flat set of scalars printed directly — and the on-disk
WAL format is hand-rolled binary by design. Adding a proc-macro dependency chain
to replace a few `println!`s would buy nothing at the boundary that matters.

`src/crc32.rs` still contains the original bitwise implementation as
`crc32::reference`. It is not used at runtime — it is kept as executable
documentation of the on-disk contract, and a differential test asserts it agrees
with `crc32fast` bit-for-bit across ~950 cases (every length 0..=300,
block-boundary lengths to 64 KiB, degenerate and pseudo-random fills, and
multi-part splits of a single stream). Known vectors are generated externally
with Python's `zlib.crc32`, so a pair of implementations that agree with each
other but are both wrong still fails.

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
cargo bench --no-run                       # compile benchmarks without running
cargo clippy --all-targets --features compare -- -D warnings
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

## Benchmarks

```powershell
cargo bench --bench engine                        # criterion suite
cargo bench --features compare --bench comparison # vs redb and sled
cargo run --release -- bench .tmp/bench 10000 256 42  # CLI end-to-end workload
```

The criterion suite covers sequential and random single writes, batched writes
across four batch sizes, point reads (hit sequential / hit random / miss),
snapshot materialization, compaction throughput, recovery time against WAL size,
and the CPU-only format and checksum path. Full results with confidence
intervals are checked in at
[`benchmarks/criterion-windows-x86_64-2026-07-28.md`](benchmarks/criterion-windows-x86_64-2026-07-28.md).

All write benchmarks are fully durable. `sync_data` runs before every `write`
returns and that is not configurable, so these numbers include the disk wait.

### Measurement environment

Intel Core Ultra 7 265U (12C/14T), 31.4 GB RAM, Samsung MZVL2512HCJQ NVMe SSD,
Windows 11 build 26200, rustc 1.96.0, release with `lto = "thin"`.

This is a power-managed laptop CPU with unpinned clocks. Run-to-run spread is
large — 11 consecutive CLI runs of the identical workload varied 1.2x on writes
and 3x on reads — and several criterion groups reported 7-13% outliers. The
numbers below are medians or criterion point estimates with their intervals; the
linked artifacts carry the full envelopes. Treat overlapping intervals as
indistinguishable rather than as a difference.

### Selected engine results

| Benchmark | Estimate | Interval |
| --- | ---: | --- |
| Batched durable writes, batch 1000 | 169,610 ops/s | 10.795-12.896 ms / 2,000 ops |
| Batched durable writes, batch 100 | 28,088 ops/s | 65.857-77.375 ms / 2,000 ops |
| Batched durable writes, batch 1 | 377 ops/s | 5.0596-5.5883 s / 2,000 ops |
| Point read, random hit | 18.386 µs | 16.338-20.686 µs |
| Point read, miss (index only) | 661.96 ns | 567.75-760.49 ns |
| Snapshot, 10,000 keys | 94.008 ms | 82.257-109.94 ms |
| Reopen 50,000 keys (14 MB WAL) | 115.46 ms | 98.681-139.37 ms |
| CRC-32, 4 KiB, accelerated | 13.17 GiB/s | 268.76-311.93 ns |
| CRC-32, 4 KiB, reference | 80.7 MiB/s | 38.916-59.649 µs |

Throughput scales nearly linearly with batch size across three orders of
magnitude (377 → 169,610 ops/s for batch 1 → 1000). That is the clearest
confirmation that one batch is one record and one fsync: at small batch sizes
almost all per-operation cost is synchronization. Callers who want throughput
must batch.

A read miss is ~28x faster than a random hit and touches no I/O, so roughly 96%
of a point read is the positional read plus the value allocation and only ~4% is
the index lookup.

### Comparison against redb and sled

`--features compare` benchmarks against [redb](https://github.com/cberner/redb)
2.6.3 and [sled](https://github.com/spacejam/sled) 0.34.7. Both are pure Rust
and build on Windows/MSVC with no C++ toolchain. **`rocksdb` was deliberately not
used**: `librocksdb-sys` needs a C++ toolchain and is slow and fragile on
Windows/MSVC, which would make the suite unrunnable for most readers.

Durability was matched before measuring, which is where this kind of comparison
usually goes wrong. redb defaults to `Durability::Immediate` (fsync per commit),
so it is compared as shipped. sled defaults to `flush_every_ms(500)` and returns
from `insert` *before* data is durable, so it is measured twice: `sled-durable`
(`flush_every_ms(None)` plus explicit `flush()`) for the fair comparison, and
`sled-default` stock purely to show the size of the gap.

| Axis | StrataDB | redb | sled (durable) | Winner |
| --- | ---: | ---: | ---: | --- |
| Individual durable writes (300 ops) | **389 ops/s** | 248 ops/s | 324 ops/s | **StrataDB** (1.6x / 1.2x) |
| Batched durable writes (5,000 ops, batch 100) | **31.5 Kops/s** | 18.1 Kops/s | 15.0 Kops/s | **StrataDB** (1.7x / 2.1x) |
| Random point reads (20,000 keys) | 26 Kelem/s | 206 Kelem/s | **284 Kelem/s** | **sled** — StrataDB ~11x slower |
| Open existing 50,000-key database | 486 ms | **9.8 ms** | 39.6 ms | **redb** — StrataDB ~50x slower |
| Range scans / ordered iteration | not implemented | yes | yes | **redb, sled** |

For reference, stock `sled-default` reaches ~20,286 ops/s on the individual-write
benchmark — **~52x faster than `sled-durable`** — entirely by not waiting for the
disk. That number is why durability parity has to be established before any of
the rest of this table means anything.

**StrataDB wins on durable write throughput and loses clearly on reads and
startup. The losses are larger than the wins.**

*Why reads lose (~11x):* not the index — a miss costs ~662 ns. Each hit issues a
fresh `seek_read` syscall and allocates a new `Vec<u8>`. redb and sled keep
user-space page caches (serving repeat reads with no syscall) and return borrowed
or reference-counted bytes instead of copying. A block cache and a zero-copy read
API are the two missing features, and together they account for essentially the
whole gap.

*Why startup loses (~50x, and worsens with size):* StrataDB has no checkpoint and
no on-disk index, so the first `get` requires replaying every record, verifying
every CRC, and rebuilding the whole index. Startup is O(bytes on disk);
extrapolating, a 1 GB WAL implies roughly 9 s. redb keeps its B-tree durable on
disk and reads only the root and metadata at open. Note that redb wins here
*because it did that work during each commit*, which is part of why it is slower
on the write benchmarks — this row and the write rows are two sides of one trade.
StrataDB buys write throughput by deferring index construction to startup. A
manifest plus a periodic index checkpoint is the standard fix and is not
implemented.

### Memory-mapped reads: measured, faster, and not adopted

`memmap2` was evaluated for the read path and **rejected despite being
substantially faster.** Both halves of that are worth stating.

A standalone probe replayed the engine's read pattern (20,000 records, 256-byte
values, fixed random order, 20 passes, page cache warmed) against three variants:

| Read strategy | Median per read | vs. current |
| --- | ---: | ---: |
| `seek_read` + fresh `Vec` (current) | 7,878 ns | baseline |
| mmap + `to_vec()` (drop-in) | 415 ns | **19x faster** |
| mmap, borrowed `&[u8]` (API change) | 36 ns | **219x faster** |

So the performance case is real and large, and it confirms the read-path
diagnosis above. It was still not adopted, for two reasons.

**1. It is incompatible with segment truncation on Windows.** Recovery repairs a
torn tail by calling `set_len` on the active segment (`wal.rs`). A second probe
attempted exactly that against a mapped file and Windows refused:

```
truncate-while-mapped: FAILED -> The requested operation cannot be performed
on a file with a user-mapped section open. (os error 1224)
```

Tail repair is not an optional feature; it is the core crash-recovery guarantee
in [docs/failure-model.md](docs/failure-model.md). Adopting mmap would require
unmapping around every repair and rotation, and worse, a mapped region whose file
shrinks underneath it is undefined behavior — a `SIGBUS`/access violation, not a
catchable Rust error. Trading a deterministic recovery path for a faster read
path is the wrong trade for an engine whose entire claim is crash safety.

**2. It requires giving up `unsafe_code = "forbid"`.** `Mmap::map` is `unsafe`
because its soundness depends on no other process truncating the file — which is
not a property this crate can enforce, since the database directory is an
ordinary directory. The lint would have to be relaxed for a guarantee that cannot
actually be upheld.

The honest framing: most of the available win is in the *borrowed* variant
(36 ns), and that requires changing `Engine::get` to return a lifetime-bound
slice — a public API change and a different design, not a drop-in optimization. A
user-space block cache would recover much of the same benefit with no `unsafe`
and no interaction with truncation, and it is the better next step. It is not
implemented.

### CLI end-to-end workload

The CLI `bench` subcommand performs 10,000 inserts in 100-operation durable
batches, 10,000 seeded point reads, a full snapshot, compaction, close/reopen
recovery, and key-count validation. It prints machine-readable JSON and refuses
to overwrite a non-empty directory.

Median of 11 consecutive runs (2026-07-28), with the observed range:

| Phase | Median | Range |
| --- | ---: | --- |
| Durable batched writes | 32,073.83 ops/s | 28,804-34,743 |
| Indexed positional point reads | 42,491.46 reads/s | 28,928-86,934 |
| Materialize 10,000-key snapshot | 0.11 s | 0.06-0.21 |
| Copy-forward compaction | 0.18 s | 0.11-0.37 |
| Reopen and rebuild index | 0.06 s | 0.02-0.24 |
| Keys verified after reopen | 10,000 / 10,000 | exact |

Checked in at
[`benchmarks/windows-x86_64-2026-07-28.json`](benchmarks/windows-x86_64-2026-07-28.json).
The `read_checksum` was identical (1271738) in all 11 runs and matches the older
artifact, confirming the workload is byte-identical.

The earlier
[`benchmarks/windows-x86_64-2026-07-16.json`](benchmarks/windows-x86_64-2026-07-16.json)
is retained but **superseded**: it was a single run, and its 18,397 writes/s and
117,113 reads/s fall outside the min/max envelope measured here. That is
measurement noise and differing machine state — *not* evidence that the
`crc32fast` swap changed end-to-end throughput. At 256-byte values the checksum
went from ~2.6 µs to ~27 ns per record against a ~2.8 ms durable write, so its
effect on this workload is around 0.1% and is not resolvable at this noise level.
The CRC change is visible where CRC is actually on the critical path: recovery.

None of these are production capacity claims. Results depend heavily on
filesystem, storage cache, sync semantics, batch size, and value distribution.

## Limits

- Storage is single-writer and single-host; there is no replication,
  multi-process read-only mode, or distributed consensus.
- The index stores every key and value location in memory. Snapshots copy every
  live value into memory.
- `get` allocates a new value buffer. There is no block cache, compression,
  range scan, bloom filter, or memory-mapped I/O. Measured consequence: point
  reads are ~11x slower than sled and ~8x slower than redb, because each hit
  costs a syscall plus an allocation while they serve cached pages by reference.
- Startup is O(bytes on disk). There is no checkpoint or on-disk index, so the
  full WAL is replayed and every CRC re-verified on open. Measured consequence:
  ~486 ms to open a 50,000-key/14 MB database, ~50x slower than redb, and the
  gap grows with size.
- Automatic repair assumes the first invalid record in the active segment
  begins an unusable tail. Sealed-segment corruption requires restore from a
  known-good copy.
- Record data is synchronized, but parent-directory metadata is not explicitly
  fsynced after create/rename on every platform. See the failure model for the
  distinction between process-crash and sudden-power-loss guarantees.

## Resume bullets

- Built a from-scratch Rust embedded key-value engine with a versioned CRC-32
  WAL, bounded binary records, atomic batches, indexed positional reads,
  sequence-consistent snapshots, and OS-enforced single-writer locking.
- Implemented deterministic crash recovery and segment lifecycle management:
  active-tail truncation, sealed-segment corruption rejection, segment
  rotation, and copy-forward compaction that preserves recoverability across
  process interruption.
- Added byte-level fault injection, eight-reader concurrency tests, offline
  inspection/repair tooling, and a criterion suite showing near-linear batch
  scaling from 377 to 169.6K durable ops/s (batch 1 → 1000).
- Benchmarked against redb and sled with durability explicitly matched: 1.7x
  faster on batched durable writes, but ~11x slower on point reads and ~50x
  slower on startup, and traced both losses to the absent block cache and
  on-disk index rather than to the index structure.
- Evaluated `memmap2` for the read path, measured a 19x gain, and rejected it:
  Windows refuses to truncate a mapped file (error 1224), which would break the
  WAL tail-repair path that the crash-safety guarantee depends on.

## License

MIT
