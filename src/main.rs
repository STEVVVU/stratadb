use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use stratadb::wal::{list_segments, scan_segment, TailPolicy};
use stratadb::{Engine, Error, Result, WriteBatch};

const BENCH_BATCH_SIZE: usize = 100;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut arguments = env::args().skip(1);
    let command = arguments.next().ok_or_else(|| usage("missing command"))?;
    match command.as_str() {
        "put" => {
            let directory = path_argument(&mut arguments, "database directory")?;
            let key = string_argument(&mut arguments, "key")?;
            let value = string_argument(&mut arguments, "value")?;
            no_more_arguments(&mut arguments)?;
            let engine = Engine::open(directory)?;
            let sequence = engine.put(key.as_bytes(), value.as_bytes())?;
            println!("sequence={sequence}");
        }
        "get" => {
            let directory = path_argument(&mut arguments, "database directory")?;
            let key = string_argument(&mut arguments, "key")?;
            no_more_arguments(&mut arguments)?;
            let engine = Engine::open(directory)?;
            match engine.get(key.as_bytes())? {
                Some(value) => println!("{}", String::from_utf8_lossy(&value)),
                None => println!("not found"),
            }
        }
        "delete" => {
            let directory = path_argument(&mut arguments, "database directory")?;
            let key = string_argument(&mut arguments, "key")?;
            no_more_arguments(&mut arguments)?;
            let engine = Engine::open(directory)?;
            let sequence = engine.delete(key.as_bytes())?;
            println!("sequence={sequence}");
        }
        "stats" => {
            let directory = path_argument(&mut arguments, "database directory")?;
            no_more_arguments(&mut arguments)?;
            let engine = Engine::open(directory)?;
            print_stats(&engine)?;
        }
        "compact" => {
            let directory = path_argument(&mut arguments, "database directory")?;
            no_more_arguments(&mut arguments)?;
            let engine = Engine::open(directory)?;
            let stats = engine.compact()?;
            println!(
                "compacted sequence={} keys={} segments={} bytes={}",
                stats.sequence, stats.key_count, stats.segment_count, stats.disk_bytes
            );
        }
        "inspect" => {
            let directory = path_argument(&mut arguments, "database directory")?;
            no_more_arguments(&mut arguments)?;
            inspect(&directory)?;
        }
        "repair" => {
            let directory = path_argument(&mut arguments, "database directory")?;
            no_more_arguments(&mut arguments)?;
            let engine = Engine::open(directory)?;
            let stats = engine.stats()?;
            println!(
                "valid sequence={} keys={} segments={} discarded_tail_bytes={}",
                stats.sequence, stats.key_count, stats.segment_count, stats.recovered_tail_bytes
            );
        }
        "bench" => {
            let directory = path_argument(&mut arguments, "new benchmark directory")?;
            let operations = optional_usize(&mut arguments, 10_000, "operations")?;
            let value_bytes = optional_usize(&mut arguments, 256, "value bytes")?;
            let seed = optional_u64(&mut arguments, 42, "seed")?;
            no_more_arguments(&mut arguments)?;
            benchmark(&directory, operations, value_bytes, seed)?;
        }
        "help" | "--help" | "-h" => print_usage(),
        _ => return Err(usage(&format!("unknown command {command:?}"))),
    }
    Ok(())
}

fn inspect(directory: &Path) -> Result<()> {
    let segments = list_segments(directory)?;
    if segments.is_empty() {
        return Err(Error::InvalidInput("no WAL segments found".to_owned()));
    }
    let mut records = 0_usize;
    let mut operations = 0_usize;
    let mut bytes = 0_u64;
    for (id, path) in segments {
        let report = scan_segment(&path, TailPolicy::Strict)?;
        println!(
            "segment={id} bytes={} records={}",
            report.valid_bytes,
            report.records.len()
        );
        bytes += report.valid_bytes;
        records += report.records.len();
        for record in report.records {
            operations += record.batch.operations.len();
            println!(
                "  offset={} length={} sequence={} operations={}",
                record.offset,
                record.byte_len,
                record.batch.sequence,
                record.batch.operations.len()
            );
        }
    }
    println!("summary bytes={bytes} records={records} operations={operations}");
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn benchmark(directory: &Path, operations: usize, value_bytes: usize, seed: u64) -> Result<()> {
    if operations == 0 {
        return Err(Error::InvalidInput(
            "benchmark operations must be positive".to_owned(),
        ));
    }
    if value_bytes == 0 {
        return Err(Error::InvalidInput(
            "benchmark value size must be positive".to_owned(),
        ));
    }
    if directory.exists() && fs::read_dir(directory)?.next().transpose()?.is_some() {
        return Err(Error::InvalidInput(format!(
            "benchmark directory must be absent or empty: {}",
            directory.display()
        )));
    }
    let operations_u64 = u64::try_from(operations)
        .map_err(|_| Error::InvalidInput("benchmark operation count exceeds u64".to_owned()))?;
    let operations_u32 = u32::try_from(operations).map_err(|_| {
        Error::InvalidInput("benchmark operation count exceeds u32 for reporting".to_owned())
    })?;
    let reporting_count = f64::from(operations_u32);

    let engine = Engine::open(directory)?;
    let write_started = Instant::now();
    for start in (0..operations).step_by(BENCH_BATCH_SIZE) {
        let mut batch = WriteBatch::new();
        for number in start..operations.min(start + BENCH_BATCH_SIZE) {
            batch.put(
                benchmark_key(number),
                benchmark_value(number, value_bytes, seed),
            );
        }
        engine.write(&batch)?;
    }
    let write_seconds = write_started.elapsed().as_secs_f64();

    let read_started = Instant::now();
    let mut generator = XorShift64::new(seed);
    let mut read_checksum = 0_u64;
    for _ in 0..operations {
        let number = usize::try_from(generator.next() % operations_u64)
            .expect("bounded benchmark index fits usize");
        let value = engine
            .get(&benchmark_key(number))?
            .ok_or_else(|| Error::InvalidInput("benchmark read missed a key".to_owned()))?;
        read_checksum = read_checksum.wrapping_add(u64::from(value[0]));
    }
    let read_seconds = read_started.elapsed().as_secs_f64();

    let snapshot_started = Instant::now();
    let snapshot = engine.snapshot()?;
    let snapshot_seconds = snapshot_started.elapsed().as_secs_f64();
    if snapshot.len() != operations {
        return Err(Error::InvalidInput(
            "benchmark snapshot key count mismatch".to_owned(),
        ));
    }

    let compact_started = Instant::now();
    let compacted = engine.compact()?;
    let compact_seconds = compact_started.elapsed().as_secs_f64();
    let final_sequence = compacted.sequence;
    drop(snapshot);
    drop(engine);

    let reopen_started = Instant::now();
    let reopened = Engine::open(directory)?;
    let reopened_stats = reopened.stats()?;
    let reopen_seconds = reopen_started.elapsed().as_secs_f64();
    if reopened_stats.key_count != operations || reopened_stats.sequence != final_sequence {
        return Err(Error::InvalidInput(
            "benchmark recovery validation failed".to_owned(),
        ));
    }

    println!("{{");
    println!("  \"benchmark\": \"seeded-batched-durable-writes\",");
    println!("  \"os\": \"{}\",", env::consts::OS);
    println!("  \"arch\": \"{}\",", env::consts::ARCH);
    println!("  \"operations\": {operations},");
    println!("  \"value_bytes\": {value_bytes},");
    println!("  \"batch_size\": {BENCH_BATCH_SIZE},");
    println!("  \"seed\": {seed},");
    println!("  \"write_seconds\": {write_seconds:.6},");
    println!(
        "  \"write_operations_per_second\": {:.2},",
        reporting_count / write_seconds
    );
    println!("  \"read_seconds\": {read_seconds:.6},");
    println!(
        "  \"point_reads_per_second\": {:.2},",
        reporting_count / read_seconds
    );
    println!("  \"snapshot_seconds\": {snapshot_seconds:.6},");
    println!("  \"compaction_seconds\": {compact_seconds:.6},");
    println!("  \"reopen_recovery_seconds\": {reopen_seconds:.6},");
    println!(
        "  \"segments_after_compaction\": {},",
        reopened_stats.segment_count
    );
    println!(
        "  \"keys_verified_after_reopen\": {},",
        reopened_stats.key_count
    );
    println!("  \"read_checksum\": {read_checksum}");
    println!("}}");
    Ok(())
}

fn benchmark_key(number: usize) -> Vec<u8> {
    format!("key-{number:010}").into_bytes()
}

fn benchmark_value(number: usize, bytes: usize, seed: u64) -> Vec<u8> {
    let mut generator = XorShift64::new(seed ^ number as u64);
    let mut value = vec![0_u8; bytes];
    for byte in &mut value {
        *byte = u8::try_from(generator.next() & 0xff).expect("masked value fits u8");
    }
    value
}

struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }
}

fn print_stats(engine: &Engine) -> Result<()> {
    let stats = engine.stats()?;
    println!("sequence={}", stats.sequence);
    println!("keys={}", stats.key_count);
    println!("segments={}", stats.segment_count);
    println!("disk_bytes={}", stats.disk_bytes);
    println!("recovered_tail_bytes={}", stats.recovered_tail_bytes);
    Ok(())
}

fn string_argument(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    arguments
        .next()
        .ok_or_else(|| usage(&format!("missing {name}")))
}

fn path_argument(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<PathBuf> {
    string_argument(arguments, name).map(PathBuf::from)
}

fn optional_usize(
    arguments: &mut impl Iterator<Item = String>,
    default: usize,
    name: &str,
) -> Result<usize> {
    arguments.next().map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|error| usage(&format!("invalid {name}: {error}")))
    })
}

fn optional_u64(
    arguments: &mut impl Iterator<Item = String>,
    default: u64,
    name: &str,
) -> Result<u64> {
    arguments.next().map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|error| usage(&format!("invalid {name}: {error}")))
    })
}

fn no_more_arguments(arguments: &mut impl Iterator<Item = String>) -> Result<()> {
    if let Some(argument) = arguments.next() {
        return Err(usage(&format!("unexpected argument {argument:?}")));
    }
    Ok(())
}

fn usage(message: &str) -> Error {
    Error::InvalidInput(format!("{message}; run `stratadb help`"))
}

fn print_usage() {
    println!(
        "\
StrataDB embedded key-value engine

Usage:
  stratadb put <directory> <key> <value>
  stratadb get <directory> <key>
  stratadb delete <directory> <key>
  stratadb stats <directory>
  stratadb compact <directory>
  stratadb inspect <directory>
  stratadb repair <directory>
  stratadb bench <new-directory> [operations] [value-bytes] [seed]"
    );
}
