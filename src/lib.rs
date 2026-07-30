//! StrataDB is a crash-safe, single-writer embedded key-value engine.

mod crc32;
mod error;
pub mod format;
pub mod wal;

pub use error::{Error, Result};
