//! `StrataDB` is a crash-safe, single-writer embedded key-value engine.
//!
//! The storage engine itself — WAL record format, recovery, index, segment
//! rotation, and compaction — is implemented from scratch here. Third-party
//! crates are used only where they are strictly better than a hand-rolled
//! version and carry no design opinion: [`crc32fast`] for the checksum,
//! `criterion` for benchmarking. See the README for the full rationale.
//!
//! [`crc32`] is public so the accelerated checksum and its bitwise reference
//! implementation can both be exercised from benchmarks and downstream format
//! tooling.

pub mod crc32;
mod engine;
mod error;
pub mod format;
pub mod wal;

pub use engine::{Config, Engine, Snapshot, Stats, WriteBatch};
pub use error::{Error, Result};
pub use format::Operation;
