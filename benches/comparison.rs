//! Comparative benchmarks: `StrataDB` vs. redb vs. sled.
//!
//! Run with `cargo bench --features compare --bench comparison`.
//!
//! # Reading these numbers honestly
//!
//! These three engines are **not** the same kind of thing, and the differences
//! matter more than the timings:
//!
//! - **`StrataDB`** is a log-structured store with a full in-memory
//!   key→offset index. Every live key is in RAM; reads are one index lookup plus
//!   one positional read. It has no B-tree, no page cache, no range scans, and no
//!   MVCC. Its index must be rebuilt by replaying the entire WAL at startup.
//! - **redb** is a copy-on-write B-tree, ACID, with MVCC readers and range
//!   scans. It does not need to read the whole file at startup.
//! - **sled** is a lock-free log-structured B-link tree with its own page cache
//!   and a background flusher.
//!
//! `StrataDB` should be expected to *lose* on anything requiring ordered
//! traversal, large-dataset memory footprint, or fast startup on a big database,
//! because it deliberately trades all three away for a simpler design. Where it
//! can be competitive is single point reads (no tree descent) and raw append
//! throughput. The point of this file is to show where each of those holds and
//! where it does not — not to find a configuration where `StrataDB` wins.
//!
//! # Durability parity
//!
//! This is the single most important fairness issue, and the easiest place for a
//! comparison benchmark to quietly lie.
//!
//! - `StrataDB` calls `sync_data` before every `write` returns. Not configurable.
//! - redb defaults to `Durability::Immediate`, which fsyncs on commit. Left at
//!   the default, so it matches.
//! - sled defaults to `flush_every_ms: Some(500)`, meaning `insert` returns
//!   **before** data is durable. Comparing that against `StrataDB`'s synchronous
//!   fsync would be measuring two different guarantees and would flatter sled by
//!   roughly the cost of an fsync per operation.
//!
//! sled is therefore measured in two configurations, both reported:
//!
//! 1. `sled-durable` — `flush_every_ms(None)` plus an explicit `flush()` per
//!    operation or batch. This is the apples-to-apples comparison.
//! 2. `sled-default` — stock configuration, buffered. Included because it is what
//!    a user actually gets by default, and because the gap between the two is
//!    itself the useful number: it shows what the durability guarantee costs.
//!
//! Reads are not durability-sensitive, so the read benchmarks use each engine's
//! default configuration.

mod support;

use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use redb::{Database, TableDefinition};
use stratadb::{Engine, WriteBatch};
use support::{key, shuffled_indices, value};
use tempfile::TempDir;

const VALUE_BYTES: usize = 256;
const SEED: u64 = 42;

/// redb requires a typed table definition; raw bytes keep the comparison to the
/// storage layer rather than a serialization format.
const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("bench");

fn scratch() -> TempDir {
    tempfile::Builder::new()
        .prefix("stratadb-compare-")
        .tempdir()
        .expect("create scratch directory")
}

/// Pre-generates the workload so key/value construction is never inside a timed
/// region for any engine.
fn payloads(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..count)
        .map(|number| (key(number), value(number, VALUE_BYTES, SEED)))
        .collect()
}

/// Individual durable writes: one fsync per operation, per engine.
///
/// This is the worst case for every engine and is dominated by device sync
/// latency rather than by CPU work, so it mostly measures how many syncs each
/// design needs per logical write.
fn individual_durable_writes(criterion: &mut Criterion) {
    const OPS: usize = 300;

    let mut group = criterion.benchmark_group("compare/write-individual-durable");
    group.throughput(Throughput::Elements(OPS as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("stratadb", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                let engine = Engine::open(directory.path()).expect("open stratadb");
                let workload = payloads(OPS);

                let started = Instant::now();
                for (this_key, this_value) in &workload {
                    engine.put(this_key, this_value).expect("put");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.bench_function("redb", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                let database =
                    Database::create(directory.path().join("db.redb")).expect("create redb");
                let workload = payloads(OPS);

                let started = Instant::now();
                for (this_key, this_value) in &workload {
                    // One transaction per key, so each key costs one commit and
                    // one fsync, matching StrataDB's one-put-one-sync.
                    let transaction = database.begin_write().expect("begin write");
                    {
                        let mut table = transaction.open_table(TABLE).expect("open table");
                        table
                            .insert(this_key.as_slice(), this_value.as_slice())
                            .expect("insert");
                    }
                    transaction.commit().expect("commit");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.bench_function("sled-durable", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                let database = sled::Config::new()
                    .path(directory.path().join("db.sled"))
                    // Disable the background flusher so durability is explicit
                    // and attributable to the flush() calls below.
                    .flush_every_ms(None)
                    .open()
                    .expect("open sled");
                let workload = payloads(OPS);

                let started = Instant::now();
                for (this_key, this_value) in &workload {
                    database
                        .insert(this_key.as_slice(), this_value.as_slice())
                        .expect("insert");
                    database.flush().expect("flush");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.bench_function("sled-default", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                // Stock configuration: buffered, flushed by a background thread
                // every 500ms. NOT durability-comparable to the others.
                let database = sled::open(directory.path().join("db.sled")).expect("open sled");
                let workload = payloads(OPS);

                let started = Instant::now();
                for (this_key, this_value) in &workload {
                    database
                        .insert(this_key.as_slice(), this_value.as_slice())
                        .expect("insert");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.finish();
}

/// Batched durable writes: one fsync per batch of 100.
///
/// Amortizing the sync is where a WAL design is supposed to shine, since a batch
/// is a single append and a single sync regardless of operation count.
fn batched_durable_writes(criterion: &mut Criterion) {
    const OPS: usize = 5_000;
    const BATCH: usize = 100;

    let mut group = criterion.benchmark_group("compare/write-batched-durable");
    group.throughput(Throughput::Elements(OPS as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("stratadb", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                let engine = Engine::open(directory.path()).expect("open stratadb");
                let workload = payloads(OPS);

                let started = Instant::now();
                for chunk in workload.chunks(BATCH) {
                    let mut batch = WriteBatch::new();
                    for (this_key, this_value) in chunk {
                        batch.put(this_key.clone(), this_value.clone());
                    }
                    engine.write(&batch).expect("write batch");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.bench_function("redb", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                let database =
                    Database::create(directory.path().join("db.redb")).expect("create redb");
                let workload = payloads(OPS);

                let started = Instant::now();
                for chunk in workload.chunks(BATCH) {
                    let transaction = database.begin_write().expect("begin write");
                    {
                        let mut table = transaction.open_table(TABLE).expect("open table");
                        for (this_key, this_value) in chunk {
                            table
                                .insert(this_key.as_slice(), this_value.as_slice())
                                .expect("insert");
                        }
                    }
                    transaction.commit().expect("commit");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.bench_function("sled-durable", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let directory = scratch();
                let database = sled::Config::new()
                    .path(directory.path().join("db.sled"))
                    .flush_every_ms(None)
                    .open()
                    .expect("open sled");
                let workload = payloads(OPS);

                let started = Instant::now();
                for chunk in workload.chunks(BATCH) {
                    let mut batch = sled::Batch::default();
                    for (this_key, this_value) in chunk {
                        batch.insert(this_key.as_slice(), this_value.as_slice());
                    }
                    database.apply_batch(batch).expect("apply batch");
                    database.flush().expect("flush");
                }
                total += started.elapsed();
            }
            total
        });
    });

    group.finish();
}

/// Random point reads on a populated database.
///
/// Each engine is loaded with the same `KEYS` keys, then read in the same
/// deterministic random order. Default configuration for all three, since reads
/// do not involve durability. Fixtures are built once and shared across
/// iterations because reads do not mutate.
fn point_reads(criterion: &mut Criterion) {
    const KEYS: usize = 20_000;

    let workload = payloads(KEYS);
    let order = shuffled_indices(KEYS, SEED);

    // StrataDB fixture.
    let strata_directory = scratch();
    let strata = Engine::open(strata_directory.path()).expect("open stratadb");
    for chunk in workload.chunks(500) {
        let mut batch = WriteBatch::new();
        for (this_key, this_value) in chunk {
            batch.put(this_key.clone(), this_value.clone());
        }
        strata.write(&batch).expect("write");
    }

    // redb fixture.
    let redb_directory = scratch();
    let redb_database =
        Database::create(redb_directory.path().join("db.redb")).expect("create redb");
    {
        let transaction = redb_database.begin_write().expect("begin write");
        {
            let mut table = transaction.open_table(TABLE).expect("open table");
            for (this_key, this_value) in &workload {
                table
                    .insert(this_key.as_slice(), this_value.as_slice())
                    .expect("insert");
            }
        }
        transaction.commit().expect("commit");
    }

    // sled fixture.
    let sled_directory = scratch();
    let sled_database = sled::open(sled_directory.path().join("db.sled")).expect("open sled");
    for (this_key, this_value) in &workload {
        sled_database
            .insert(this_key.as_slice(), this_value.as_slice())
            .expect("insert");
    }
    sled_database.flush().expect("flush");

    let mut group = criterion.benchmark_group("compare/read-point-random");
    group.throughput(Throughput::Elements(1));

    group.bench_function("stratadb", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let found = strata
                .get(&workload[order[cursor % KEYS]].0)
                .expect("get")
                .expect("key present");
            cursor += 1;
            found
        });
    });

    group.bench_function("redb", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let transaction = redb_database.begin_read().expect("begin read");
            let table = transaction.open_table(TABLE).expect("open table");
            let found = table
                .get(workload[order[cursor % KEYS]].0.as_slice())
                .expect("get")
                .expect("key present")
                .value()
                .to_vec();
            cursor += 1;
            found
        });
    });

    group.bench_function("sled", |bencher| {
        let mut cursor = 0_usize;
        bencher.iter(|| {
            let found = sled_database
                .get(workload[order[cursor % KEYS]].0.as_slice())
                .expect("get")
                .expect("key present");
            cursor += 1;
            found
        });
    });

    group.finish();
}

/// Startup / recovery time on an already-populated database.
///
/// This is `StrataDB`'s structural weak point and the reason it is measured
/// separately. `StrataDB` must replay every WAL record and verify every CRC to
/// rebuild its in-memory index, so open cost is linear in total bytes on disk.
/// redb and sled read metadata and page in lazily, so their open cost is close to
/// constant. Expect `StrataDB` to lose here by a wide and *growing* margin.
fn open_existing(criterion: &mut Criterion) {
    const KEYS: usize = 50_000;

    let workload = payloads(KEYS);

    let strata_directory = scratch();
    {
        let strata = Engine::open(strata_directory.path()).expect("open stratadb");
        for chunk in workload.chunks(500) {
            let mut batch = WriteBatch::new();
            for (this_key, this_value) in chunk {
                batch.put(this_key.clone(), this_value.clone());
            }
            strata.write(&batch).expect("write");
        }
    }

    let redb_directory = scratch();
    let redb_path = redb_directory.path().join("db.redb");
    {
        let database = Database::create(&redb_path).expect("create redb");
        let transaction = database.begin_write().expect("begin write");
        {
            let mut table = transaction.open_table(TABLE).expect("open table");
            for (this_key, this_value) in &workload {
                table
                    .insert(this_key.as_slice(), this_value.as_slice())
                    .expect("insert");
            }
        }
        transaction.commit().expect("commit");
    }

    let sled_directory = scratch();
    let sled_path = sled_directory.path().join("db.sled");
    {
        let database = sled::open(&sled_path).expect("open sled");
        for (this_key, this_value) in &workload {
            database
                .insert(this_key.as_slice(), this_value.as_slice())
                .expect("insert");
        }
        database.flush().expect("flush");
    }

    let mut group = criterion.benchmark_group("compare/open-existing-50k");
    group.sample_size(10);

    group.bench_function("stratadb", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let started = Instant::now();
                let engine = Engine::open(strata_directory.path()).expect("reopen stratadb");
                total += started.elapsed();
                drop(engine);
            }
            total
        });
    });

    group.bench_function("redb", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let started = Instant::now();
                let database = Database::open(&redb_path).expect("reopen redb");
                total += started.elapsed();
                drop(database);
            }
            total
        });
    });

    group.bench_function("sled", |bencher| {
        bencher.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let started = Instant::now();
                let database = sled::open(&sled_path).expect("reopen sled");
                total += started.elapsed();
                drop(database);
            }
            total
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    individual_durable_writes,
    batched_durable_writes,
    point_reads,
    open_existing
);
criterion_main!(benches);
