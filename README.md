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
| `crc32fast` | 1.5 | Hardware-accelerated CRC-32/ISO-HDLC (PCLMULQDQ). Measured **roughly two orders of magnitude faster** than the previous scalar implementation — ~95x at 256 B (interval-derived range 71-125x) and ~190x at 64 KiB (147-245x). Same polynomial, same output. A real engine would not ship a bitwise CRC. |
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
cargo test --locked --all-targets
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo bench --no-run                       # compile benchmarks without running
cargo clippy --locked --all-targets --features compare -- -D warnings
```

The suite is 18 tests. CI asserts that as a **floor** rather than trusting
`cargo test`'s exit code, because `cargo test` exits 0 when it discovers nothing —
so a refactor that stopped tests being collected would otherwise leave CI green
while testing nothing. The check sums the reported counts with `awk` (not `bc`,
which is not installed on the Windows runner) and fails below the floor; raise it
when tests are added. It was verified by deliberately removing tests and
confirming it goes red, including the case where tests silently stop being
discovered but `cargo test` still reports success.

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
cargo bench --bench engine -- read/path-decomposition   # just the read breakdown
cargo bench --features compare --bench comparison # vs redb and sled
cargo run --release -- bench .tmp/bench 10000 256 42  # CLI end-to-end workload
```

The criterion suite covers sequential and random single writes, batched writes
across four batch sizes, point reads (hit sequential / hit random / miss), a
four-way read-path decomposition that apportions a read between its syscall and
its allocation, snapshot materialization, compaction throughput, recovery time
against WAL size, and the CPU-only format and checksum path. Full results with
confidence intervals are checked in at
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

A read miss touches no I/O and is roughly **20-35x faster** than a random hit
(point estimates 662 ns vs 18.4 µs give ~28x; propagating both intervals gives
21-36x). So the large majority of a point read — on the order of 95% — is the
positional read rather than the index lookup. The
`read/path-decomposition` group below pins down which part of that remainder is
the syscall and which is the allocation.

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

Every figure below carries criterion's confidence interval, and every ratio
carries a range derived from **both** intervals (slowest-over-fastest to
fastest-over-slowest) rather than being quoted as a bare midpoint-over-midpoint
number. A ratio range that straddles 1.0x would mean "not distinguished."

| Axis | StrataDB | redb | sled (durable) | Winner |
| --- | ---: | ---: | ---: | --- |
| Individual durable writes (300 ops) | **389 ops/s**<br>377-400 | 248 ops/s<br>234-259 | 324 ops/s<br>294-352 | **StrataDB**<br>1.6x redb (1.5-1.7)<br>1.2x sled (1.1-1.4) |
| Batched durable writes (5,000 ops, batch 100) | **31.5 Kops/s**<br>28.4-35.3 | 18.1 Kops/s<br>17.2-18.7 | 15.0 Kops/s<br>14.2-16.0 | **StrataDB**<br>1.7x redb (1.5-2.1)<br>2.1x sled (1.8-2.5) |
| Random point reads (20,000 keys) | 26 Kelem/s<br>22-31 | 206 Kelem/s<br>197-216 | **284 Kelem/s**<br>270-299 | **sled** — StrataDB<br>~11x slower (8.6-13.5) |
| Open existing 50,000-key database | 486 ms<br>405-580 | **9.8 ms**<br>8.9-11.7 | 39.6 ms<br>36.3-42.6 | **redb** — StrataDB<br>~50x slower (34-65) |
| Range scans / ordered iteration | not implemented | yes | yes | **redb, sled** |

Throughput bounds are derived from the time intervals in the linked artifact
(fastest time gives the highest ops/s), so they are that same measurement read
the other way round, not a second experiment.

For reference, stock `sled-default` reaches ~20,300 ops/s on the individual-write
benchmark (interval 17,200-30,600 ops/s, from 9.80-17.48 ms for 300 ops) —
roughly **50-100x faster than `sled-durable`**, entirely by not waiting for the
disk. The ratio range is wide because `sled-default` is fast enough that its own
interval is proportionally large; the point survives at either end. That gap is
why durability parity has to be established before any of the rest of this table
means anything.

**StrataDB wins on durable write throughput and loses clearly on reads and
startup. The losses are larger than the wins.**

*Why reads lose (~11x, range 8.6-13.5x):* not the index — a miss costs ~662 ns.
Each hit issues a fresh `seek_read` syscall and allocates a new `Vec<u8>`. redb
and sled keep user-space page caches (serving repeat reads with no syscall) and
return borrowed or reference-counted bytes instead of copying. The
`read/path-decomposition` benchmark below separates those two costs and finds the
**syscall is essentially the entire gap (~9x)** while the allocation is not
resolvable at this noise level — so a block cache is the fix and a zero-copy read
API is a smaller follow-on, not a co-equal cause.

*Why startup loses (~50x, range 34-65x, and worsens with size):* StrataDB has no checkpoint and
no on-disk index, so the first `get` requires replaying every record, verifying
every CRC, and rebuilding the whole index. Startup is O(bytes on disk);
extrapolating, a 1 GB WAL implies roughly 9 s. redb keeps its B-tree durable on
disk and reads only the root and metadata at open. Note that redb wins here
*because it did that work during each commit*, which is part of why it is slower
on the write benchmarks — this row and the write rows are two sides of one trade.
StrataDB buys write throughput by deferring index construction to startup. A
manifest plus a periodic index checkpoint is the standard fix and is not
implemented.

### Where the read cost actually goes, and why mmap was still rejected

An earlier revision of this README justified the read-path diagnosis with an mmap
probe reporting 7,878 ns / 415 ns / 36 ns per read (a "19x" drop-in gain and
"219x" borrowed-slice gain). **Those figures are withdrawn.** The probe was never
committed, so nothing in this repository reproduces them and no reader could
check them. They are not restated here or anywhere else in the repo.

What replaces them is a benchmark that is checked in and runnable —
`read/path-decomposition` in [`benches/engine.rs`](benches/engine.rs) — which
splits a point read into the costs the argument actually depends on. It uses only
safe code and no new dependency:

```powershell
cargo bench --bench engine -- read/path-decomposition
```

That command measures the four variants **once**, and reports criterion's own
within-run intervals. The table below is not that output: it is a median over 11
whole-process runs with per-run paired ratios, because the between-run spread on
this workload is wider than any single run's interval. That aggregation has its own
committed driver, so the table is reproducible rather than resting on a one-off
script:

```powershell
python scripts/paired_read_decomposition.py --runs 11
```

It runs the bench N times, reads each run's `median.point_estimate` out of
criterion's `estimates.json`, computes the per-variant medians, the per-run paired
ratios against `current`, the min/max envelope, and the win count out of N, then
writes `benchmarks/read-path-decomposition.json`. It aborts rather than emitting a
short table if a run fails, if a variant is missing, or if criterion did not
rewrite its output.

The figures below are from an 11-run pass on the machine described above and have
not been re-measured since. Absolute microsecond values are machine-specific and
drift run-to-run here by up to ~2x, so expect a fresh pass to reproduce the
*structure* — syscall removal worth ~9x and winning every run, allocation removal
lost in the noise — rather than the exact absolutes. Ratios are computed **per run
and then aggregated**, so machine-state drift cancels instead of inflating them:

| Variant | Median | Range over 11 runs | vs. `current` (paired) |
| --- | ---: | --- | --- |
| `current` — index + `seek_read` + fresh `Vec` | 14.67 µs | 10.82-30.73 µs | baseline |
| `reused-buffer` — same syscall, one shared buffer | 13.94 µs | 10.62-66.01 µs | 1.05x (0.40-2.67x) |
| `cached-copy` — in-process bytes, fresh `Vec` | 1.70 µs | 1.18-8.27 µs | **9.2x (1.4-21.3x)** |
| `cached-borrow` — in-process bytes, borrowed `&[u8]` | 1.20 µs | 0.54-3.54 µs | **11.7x (4.8-27.8x)** |

Two results, and the second one corrects this README's previous explanation:

**The syscall is the cost.** Removing it is worth ~9x (median; every one of 11
runs improved), and that is the whole gap against sled. This is the part of the
old claim that survives — the direction and the rough order of magnitude — though
at 9.2x rather than the withdrawn "19x."

**The allocation is not measurable here.** `reused-buffer` keeps the syscall and
drops the per-value allocation, and it lands at 1.05x with a range spanning 1.0x
(0.40-2.67x, 7 of 11 runs above 1x). That is indistinguishable from no change.
Earlier text in this file attributed the read loss to "a syscall plus an
allocation"; only the syscall is supported by measurement. Going further,
`cached-borrow` beats `cached-copy` by just 1.27x (0.66-3.35x) — so a zero-copy
API is a real but *secondary* win, not the dominant one the withdrawn 36 ns
figure implied. A block cache is the fix; zero-copy is a refinement on top.

**Why the same read is quoted at three different times.** Three tables in this repo
measure "a random point read hit" and disagree, so it is worth saying why rather
than leaving it to look like a contradiction:

| Where | Figure | Bench |
| --- | ---: | --- |
| Selected engine results, above | 18.386 µs | `read/point/hit/random` (`benches/engine.rs`) |
| Comparison table, above | 38.321 µs | `compare/read-point-random` → `stratadb` (`benches/comparison.rs`) |
| Decomposition table, above | 14.67 µs | `read/path-decomposition` → `current` (`benches/engine.rs`) |

The workload is *not* the variable. All three use 20,000 keys, 256-byte values,
`SEED = 42`, and the same deterministic shuffle from `benches/support/mod.rs`, and
all three call the same `Engine::get`. What differs is the **process each one is
measured in**:

- **18.386 µs vs 38.321 µs** — different bench binaries with different resident
  fixtures. `compare/read-point-random` builds and holds StrataDB, redb, *and* sled
  fixtures live in one process (`benches/comparison.rs`), each with its own caches
  and background threads; sled in particular keeps its own page cache. StrataDB's
  reads there contend for OS page cache against two other engines' working sets, so
  more of them miss and reach the disk. `read/point/hit/random` runs in the `engine`
  binary with one fixture and nothing competing for cache.
- **18.386 µs vs 14.67 µs** — different aggregation, not a different measurement.
  14.67 µs is a **median over 11 whole-process runs**; 18.386 µs is a single run's
  point estimate. The 11-run range for `current` is 10.82-30.73 µs, which contains
  18.386 µs — the two agree. (The decomposition group also holds every segment in
  RAM as `Vec<u8>` images for the `cached-*` variants, which warms the same pages
  `current` reads.)

So the numbers are comparable **within** a table and not **across** tables. The
only cross-group reasoning done above is the ~11x comparison loss against the
decomposition's ~9x syscall share, and that is a comparison of *ratios measured
inside their own groups*, not of absolute microseconds across groups.

**On what `cached-*` is and is not.** Those two variants hold the segments in an
ordinary `Vec<u8>`, which stands in for a **block cache, not for mmap**. It shares
mmap's removal of the read syscall but not its demand paging, and it skips the
eviction and residency bookkeeping a real cache must pay. So it bounds what
removing the syscall can buy on resident data. It is deliberately **not** offered
as a measurement of `memmap2`, and the numbers above are not labelled as one.

mmap was still evaluated for the read path and rejected, for two reasons that do
not depend on any timing figure.

**1. It is incompatible with segment truncation on Windows.** Recovery repairs a
torn tail by calling `set_len` on the active segment (`wal.rs`). A probe attempted
exactly that against a mapped file and Windows refused:

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

The honest framing, now that the decomposition above has been measured: a
**user-space block cache is the right next step**, and it needs no `unsafe`, no
new dependency, and has no interaction with truncation. It captures the ~9x that
removing the syscall is worth. A zero-copy `Engine::get` returning a
lifetime-bound slice is a further ~1.3x on top and is a public API change; the
decomposition says it is a refinement, not the main event. Neither is implemented.

### CLI end-to-end workload

The CLI `bench` subcommand performs 10,000 inserts in 100-operation durable
batches, 10,000 seeded point reads, a full snapshot, compaction, close/reopen
recovery, and key-count validation. It prints machine-readable JSON and refuses
to overwrite a non-empty directory.

**Use `--out <file>` to capture the report, not a shell redirect.** The binary
writes the file itself as UTF-8 with LF endings and no BOM:

```powershell
cargo run --release -- bench .tmp/bench 10000 256 42 --out benchmarks/run.json
```

A bare `> run.json` works in `bash` and in PowerShell 7+, but **PowerShell 5.1 —
still the default `powershell.exe` on Windows — encodes redirected output as
UTF-16LE with a byte-order mark**, and so does `Tee-Object`. Verified on this
machine: `>` produced a file beginning `ff fe 7b 00` with CRLF line endings, which
no JSON parser will read, while `--out` produced `7b 0a` and zero CR bytes. A
sibling project in this portfolio had to fix exactly this after committing an
unparseable "JSON" artifact, which is why the flag exists rather than a
documentation note alone.

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
  reads are ~11x slower than sled (range 8.6-13.5x) and ~8x slower than redb
  (6.3-9.8x), because each hit costs a positional-read syscall while they serve
  cached pages with none. The syscall is the dominant term (~9x); the allocation
  is not measurable at this noise level.
- Startup is O(bytes on disk). There is no checkpoint or on-disk index, so the
  full WAL is replayed and every CRC re-verified on open. Measured consequence:
  ~486 ms to open a 50,000-key/14 MB database, ~50x slower than redb (range
  34-65x), and the gap grows with size.
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
  on-disk index rather than to the index structure. Every ratio is reported with
  a range derived from both engines' confidence intervals.
- Decomposed the read-path loss with a committed benchmark, isolating the
  positional-read syscall as ~9x of it (median of 11 runs) and showing the
  per-value allocation to be indistinguishable from noise; evaluated `memmap2` as
  the fix and rejected it because Windows refuses to truncate a mapped file
  (error 1224), which would break the WAL tail-repair path the crash-safety
  guarantee depends on.

## License

MIT
