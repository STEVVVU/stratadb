//! Criterion benchmarks for the `StrataDB` engine.
//!
//! ## Why most of these use `iter_custom`
//!
//! Writes, compaction, and recovery are *destructive and stateful*: the second
//! iteration of "write 1000 keys" runs against a larger database than the first,
//! and the second iteration of "compact" has nothing left to compact. Timing
//! them with criterion's plain `iter` would measure a workload that drifts as
//! the sample progresses.
//!
//! `iter_custom` hands us the iteration count and asks for the total elapsed
//! time, which lets each iteration rebuild its own fixture *outside* the
//! accumulated duration. Only the operation under test is timed.
//!
//! ## Durability is not disabled
//!
//! Every measured write goes through the real `sync_data` path. The absolute
//! numbers are therefore dominated by device fsync latency, which is the honest
//! thing to report for a WAL engine: it is what a caller actually waits for.
//! Benchmarks that disable fsync measure a database nobody would trust.
//!
//! ## Sample sizes
//!
//! The fsync-bound groups use reduced sample sizes and explicit measurement
//! windows. This is a deliberate trade of statistical tightness for a suite that
//! finishes; criterion still reports the full distribution and its own
//! confidence intervals, so variance stays visible rather than being hidden
//! behind a single number.

mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use stratadb::{Config, Engine, WriteBatch};
use support::{key, shuffled_indices, value};
use tempfile::TempDir;

/// Value payload size, matching the checked-in CLI benchmark artifact.
const VALUE_BYTES: usize = 256;
/// Fixed seed so every run generates byte-identical keys and values.
const SEED: u64 = 42;

fn scratch() -> TempDir {
    tempfile::Builder::new()
        .prefix("stratadb-bench-")
        .tempdir()
        .expect("create scratch directory")
}

/// Populates a fresh engine with `count` keys using batched writes.
///
/// Used as untimed fixture setup. Batched so fixture construction is not itself
/// dominated by fsync latency.
fn populate(directory: &std::path::Path, count: usize) -> Engine {
    let engine = Engine::open(directory).expect("open engine");
    for start in (0..count).step_by(500) {
        let mut batch = WriteBatch::new();
        for number in start..count.min(start + 500) {
            batch.put(key(number), value(number, VALUE_BYTES, SEED));
        }
        engine.write(&batch).expect("populate write");
    }
    engine
}

/// Single-key durable writes, sequential and random key order.
///
/// Each iteration writes `OPS` keys into a freshly created database, one `put`
/// (and therefore one fsync) per key. The sequential/random split exists to show
/// what the access pattern costs: this engine appends to a log regardless of key
/// order, so the two are expected to be nearly equal — unlike a B-tree, where
/// random insertion order causes page splits. A large gap here would indicate an
/// unexpected ordering dependency.
fn single_writes(criterion: &mut Criterion) {
    const OPS: usize = 500;

    let mut group = criterion.benchmark_group("write/single-durable");
    group.throughput(Throughput::Elements(OPS as u64));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(15));

    group.bench_function("sequential", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                let engine = Engine::open(directory.path()).expect("open engine");
                let payloads: Vec<(Vec<u8>, Vec<u8>)> = (0..OPS)
                    .map(|number| (key(number), value(number, VALUE_BYTES, SEED)))
                    .collect();

                let started = Instant::now();
                for (this_key, this_value) in &payloads {
                    engine.put(this_key, this_value).expect("put");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.bench_function("random", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                let engine = Engine::open(directory.path()).expect("open engine");
                let payloads: Vec<(Vec<u8>, Vec<u8>)> = shuffled_indices(OPS, SEED)
                    .into_iter()
                    .map(|number| (key(number), value(number, VALUE_BYTES, SEED)))
                    .collect();

                let started = Instant::now();
                for (this_key, this_value) in &payloads {
                    engine.put(this_key, this_value).expect("put");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.finish();
}

/// Batched durable writes across batch sizes.
///
/// One batch is one WAL record and one fsync, so this group isolates how much of
/// the single-write cost is synchronization rather than encode/append work.
/// Throughput is reported per operation, so the batch-size curve is directly
/// comparable across the group.
fn batch_writes(criterion: &mut Criterion) {
    const OPS: usize = 2_000;

    let mut group = criterion.benchmark_group("write/batched-durable");
    group.throughput(Throughput::Elements(OPS as u64));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(12));

    for batch_size in [1_usize, 10, 100, 1_000] {
        group.bench_function(format!("batch-{batch_size}"), |bencher| {
            bencher.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let directory = scratch();
                    let engine = Engine::open(directory.path()).expect("open engine");
                    let batches: Vec<WriteBatch> = (0..OPS)
                        .step_by(batch_size)
                        .map(|start| {
                            let mut batch = WriteBatch::new();
                            for number in start..OPS.min(start + batch_size) {
                                batch.put(key(number), value(number, VALUE_BYTES, SEED));
                            }
                            batch
                        })
                        .collect();

                    let started = Instant::now();
                    for batch in &batches {
                        engine.write(batch).expect("write batch");
                    }
                    total += started.elapsed();
                }
                total
            });
        });
    }

    group.finish();
}

/// Point reads against a populated database.
///
/// Reads are non-destructive, so the fixture is built once per benchmark and
/// reused; only the lookups are timed. Each read is an index lookup plus one
/// positional read (`seek_read`/`read_at`) and a fresh allocation for the value.
///
/// The `hit/random` and `hit/sequential` split shows the OS page-cache
/// contribution: sequential key order walks the segment file in offset order,
/// random order does not. `miss` measures the index-only path with no I/O at
/// all, which bounds how much of a hit is lookup versus read.
fn point_reads(criterion: &mut Criterion) {
    const KEYS: usize = 20_000;

    let directory = scratch();
    let engine = populate(directory.path(), KEYS);

    let sequential: Vec<Vec<u8>> = (0..KEYS).map(key).collect();
    let random: Vec<Vec<u8>> = shuffled_indices(KEYS, SEED).into_iter().map(key).collect();
    let absent: Vec<Vec<u8>> = (0..KEYS)
        .map(|number| format!("absent-{number:010}").into_bytes())
        .collect();

    let mut group = criterion.benchmark_group("read/point");
    group.throughput(Throughput::Elements(1));

    group.bench_function("hit/sequential", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let found = engine.get(&sequential[cursor % KEYS]).expect("get");
            cursor += 1;
            found
        });
    });

    group.bench_function("hit/random", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let found = engine.get(&random[cursor % KEYS]).expect("get");
            cursor += 1;
            found
        });
    });

    group.bench_function("miss", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let found = engine.get(&absent[cursor % KEYS]).expect("get");
            cursor += 1;
            found
        });
    });

    group.finish();
}

/// Decomposes a point read into syscall cost, allocation cost, and index cost.
///
/// ## Why this benchmark exists
///
/// The comparison suite shows point reads losing ~11x to sled, and the
/// explanation offered for that loss is "each hit costs a `seek_read` syscall
/// plus a fresh `Vec` allocation, and redb/sled pay neither on a cache hit."
/// That is a causal claim about where the time goes, so it needs a measurement
/// that separates those costs rather than an assertion.
///
/// An earlier revision of the README justified the same claim with an mmap probe
/// that was never committed and cannot be reproduced; those figures have been
/// withdrawn. This group replaces them with something a reader can run, and it
/// deliberately measures the decomposition using **only safe code and no new
/// dependency**:
///
/// - `current` — the shipped `Engine::get`: index lookup, positional read
///   syscall, fresh `Vec<u8>` per value. The baseline.
/// - `reused-buffer` — same index lookup and same positional read syscall, but
///   into one pre-allocated buffer. The delta against `current` is the value
///   allocation and nothing else.
/// - `cached-copy` — value copied out of an in-process byte image of the segment
///   into a fresh `Vec`. No syscall; still allocates. This is what a user-space
///   block cache would cost on a hit.
/// - `cached-borrow` — the same in-process image, returning a borrowed `&[u8]`.
///   No syscall and no allocation: the floor for this index and this key order.
///
/// ## What this does and does not measure
///
/// The two `cached-*` variants hold the whole segment set in an ordinary
/// `Vec<u8>`, which is a *stand-in* for a block cache, not for mmap. It shares
/// mmap's property of removing the read syscall, but not mmap's demand paging or
/// its avoidance of a userspace copy of the file. So `cached-borrow` bounds what
/// removing the syscall and the allocation can buy on fully-resident data; it is
/// not a measurement of `memmap2`, and it is not quoted as one. It is also an
/// upper bound in the other direction: a real block cache has to pay for
/// residency accounting and eviction, which this does not.
///
/// The index used by the three non-`current` variants is rebuilt here from the
/// public `wal::scan_segment` output using the same offset arithmetic the engine
/// uses at recovery, so all four variants resolve the same key to the same bytes.
fn read_path_decomposition(criterion: &mut Criterion) {
    use stratadb::format::HEADER_LEN;
    use stratadb::wal::{list_segments, scan_segment, TailPolicy};

    /// Absolute location of one value payload within a segment.
    #[derive(Clone, Copy)]
    struct Located {
        segment_id: u64,
        offset: u64,
        len: usize,
    }

    const KEYS: usize = 20_000;

    let directory = scratch();
    let engine = populate(directory.path(), KEYS);

    // Rebuild a key -> value-location index from the committed segments, using
    // the same arithmetic as `Engine`'s recovery path: a value lives at the
    // record offset, past the fixed header, plus the payload-relative offset the
    // encoder recorded.
    let segments = list_segments(directory.path()).expect("list segments");
    let mut index: BTreeMap<Vec<u8>, Located> = BTreeMap::new();
    let mut images: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for (segment_id, path) in &segments {
        let report = scan_segment(path, TailPolicy::Strict).expect("scan segment");
        for record in report.records {
            for (operation, value_offset) in record
                .batch
                .operations
                .iter()
                .zip(&record.batch.value_offsets)
            {
                if let Some((relative, len)) = value_offset {
                    index.insert(
                        operation.key().to_vec(),
                        Located {
                            segment_id: *segment_id,
                            offset: record.offset
                                + u64::try_from(HEADER_LEN).expect("constant fits u64")
                                + u64::from(*relative),
                            len: usize::try_from(*len).expect("u32 fits usize"),
                        },
                    );
                }
            }
        }
        images.insert(
            *segment_id,
            std::fs::read(path).expect("read segment into memory"),
        );
    }
    assert_eq!(index.len(), KEYS, "rebuilt index must cover every key");

    // Separate read-only handles, so the positional-read variant does not share
    // a file cursor with the engine's own cached handles.
    let handles: BTreeMap<u64, std::fs::File> = segments
        .iter()
        .map(|(segment_id, path)| {
            (
                *segment_id,
                std::fs::File::open(path).expect("open segment for reading"),
            )
        })
        .collect();

    // Same fixed random key order for all four variants, so any difference is
    // the read strategy rather than the access pattern.
    let random: Vec<Vec<u8>> = shuffled_indices(KEYS, SEED).into_iter().map(key).collect();

    let mut group = criterion.benchmark_group("read/path-decomposition");
    group.throughput(Throughput::Elements(1));

    group.bench_function("current", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let found = engine.get(&random[cursor % KEYS]).expect("get");
            cursor += 1;
            found
        });
    });

    group.bench_function("reused-buffer", |bencher| {
        let mut cursor = 0_usize;
        let mut buffer = vec![0_u8; VALUE_BYTES];
        bencher.iter(|| {
            let located = index[&random[cursor % KEYS]];
            cursor += 1;
            let file = &handles[&located.segment_id];
            read_exact_at(file, &mut buffer[..located.len], located.offset);
            std::hint::black_box(buffer[0])
        });
    });

    group.bench_function("cached-copy", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let located = index[&random[cursor % KEYS]];
            cursor += 1;
            let image = &images[&located.segment_id];
            let start = usize::try_from(located.offset).expect("offset fits usize");
            image[start..start + located.len].to_vec()
        });
    });

    group.bench_function("cached-borrow", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let located = index[&random[cursor % KEYS]];
            cursor += 1;
            let image = &images[&located.segment_id];
            let start = usize::try_from(located.offset).expect("offset fits usize");
            std::hint::black_box(&image[start..start + located.len])[0]
        });
    });

    group.finish();
}

/// Positional read that does not disturb the file cursor, mirroring the
/// `read_exact_at` helper in `src/engine.rs`.
///
/// Duplicated here rather than exported because it is a private implementation
/// detail of the engine; the benchmark needs the same syscall shape to make the
/// `current` / `reused-buffer` comparison meaningful.
fn read_exact_at(file: &std::fs::File, buffer: &mut [u8], offset: u64) {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt as _;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt as _;

    let mut read = 0_usize;
    while read < buffer.len() {
        let at = offset + u64::try_from(read).expect("buffer offset fits u64");
        #[cfg(windows)]
        let count = file.seek_read(&mut buffer[read..], at).expect("seek_read");
        #[cfg(unix)]
        let count = file.read_at(&mut buffer[read..], at).expect("read_at");
        assert!(count > 0, "unexpected end of segment while reading a value");
        read += count;
    }
}

/// Full snapshot materialization.
///
/// `snapshot` copies every live value into an immutable map, so its cost scales
/// with live data size, not with the number of keys touched since the last
/// snapshot. Reported per key so the scaling is readable across sizes.
fn snapshots(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("snapshot/materialize");
    group.sample_size(30);

    for keys in [1_000_usize, 10_000] {
        let directory = scratch();
        let engine = populate(directory.path(), keys);

        group.throughput(Throughput::Elements(keys as u64));
        group.bench_function(format!("{keys}-keys"), |bencher| {
            bencher.iter(|| engine.snapshot().expect("snapshot"));
        });
    }

    group.finish();
}

/// Copy-forward compaction throughput.
///
/// Each iteration needs a database that still has garbage to reclaim, so the
/// fixture writes `VERSIONS` copies of every key (only the last survives) and is
/// rebuilt per iteration outside the timed region. Throughput is reported in
/// bytes of live data written by compaction, which is the work compaction
/// actually performs.
fn compaction(criterion: &mut Criterion) {
    const KEYS: usize = 5_000;
    const VERSIONS: usize = 3;

    let mut group = criterion.benchmark_group("compaction");
    group.throughput(Throughput::Bytes((KEYS * VALUE_BYTES) as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function(format!("{KEYS}-keys-{VERSIONS}-versions"), |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                // A modest segment target forces real rotation, so compaction
                // has multiple sealed segments to fold together rather than a
                // single file.
                let engine = Engine::open_with_config(
                    directory.path(),
                    Config {
                        max_segment_bytes: 4 * 1024 * 1024,
                    },
                )
                .expect("open engine");
                for version in 0..VERSIONS {
                    for start in (0..KEYS).step_by(500) {
                        let mut batch = WriteBatch::new();
                        for number in start..KEYS.min(start + 500) {
                            batch.put(
                                key(number),
                                value(number + version * KEYS, VALUE_BYTES, SEED),
                            );
                        }
                        engine.write(&batch).expect("write");
                    }
                }

                let started = Instant::now();
                engine.compact().expect("compact");
                total += started.elapsed();
            }
            total
        });
    });

    group.finish();
}

/// Recovery time as a function of WAL size.
///
/// This is the metric that matters for restart latency: recovery replays every
/// record in every segment, verifies each CRC, and rebuilds the whole index, so
/// the cost is linear in bytes on disk rather than in live key count. Reported
/// per byte of WAL so the linearity is checkable across sizes.
///
/// The fixture is built once per size and reopened repeatedly. Reopening is
/// idempotent here — there is no corruption to repair, so no iteration mutates
/// the segments — but the engine holds an exclusive lock, so each open must be
/// dropped before the next. `iter_custom` times only the open.
fn recovery(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("recovery/reopen");
    group.sample_size(20);

    for keys in [1_000_usize, 10_000, 50_000] {
        let directory = scratch();
        let wal_bytes = {
            let engine = populate(directory.path(), keys);
            let bytes = engine.stats().expect("stats").disk_bytes;
            drop(engine);
            bytes
        };

        group.throughput(Throughput::Bytes(wal_bytes));
        group.bench_function(format!("{keys}-keys-{wal_bytes}-bytes"), |bencher| {
            bencher.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let started = Instant::now();
                    let engine = Engine::open(directory.path()).expect("reopen engine");
                    total += started.elapsed();
                    // Dropping releases the writer lock. Kept outside the timed
                    // region so close cost is not attributed to recovery.
                    drop(engine);
                }
                total
            });
        });
    }

    group.finish();
}

/// Record encode/decode and checksum cost, with no I/O.
///
/// This isolates the CPU-side cost of the format from device latency, which is
/// the only place the `crc32fast` swap is visible at all. Both the accelerated
/// and the reference CRC implementations are measured to quantify what that swap
/// bought; the reference is not used at runtime.
fn format_and_checksum(criterion: &mut Criterion) {
    use stratadb::crc32;
    use stratadb::format::{encode_batch, Operation};

    let mut group = criterion.benchmark_group("format");

    for size in [256_usize, 4_096, 65_536] {
        let payload = value(0, size, SEED);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_function(format!("crc32/accelerated/{size}B"), |bencher| {
            bencher.iter(|| crc32::checksum(&[&payload]));
        });
        group.bench_function(format!("crc32/reference/{size}B"), |bencher| {
            bencher.iter(|| crc32::reference::checksum(&[&payload]));
        });
    }

    let operations: Vec<Operation> = (0..100)
        .map(|number| Operation::Put {
            key: key(number),
            value: value(number, VALUE_BYTES, SEED),
        })
        .collect();
    group.throughput(Throughput::Elements(operations.len() as u64));
    group.bench_function("encode-batch/100-ops", |bencher| {
        bencher.iter_batched(
            || operations.clone(),
            |operations| encode_batch(1, &operations).expect("encode"),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(
    benches,
    single_writes,
    batch_writes,
    point_reads,
    read_path_decomposition,
    snapshots,
    compaction,
    recovery,
    format_and_checksum
);
criterion_main!(benches);
