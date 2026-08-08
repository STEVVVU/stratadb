use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

#[cfg(not(any(unix, windows)))]
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
#[cfg(windows)]
use std::os::windows::fs::FileExt as _;

use crate::format::{
    encode_batch, EncodedBatch, Operation, HEADER_LEN, MAX_BATCH_OPERATIONS, MAX_RECORD_BYTES,
};
use crate::wal::{list_segments, scan_segment, segment_path, TailPolicy};
use crate::{Error, Result};

const DEFAULT_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
const LOCK_FILE: &str = ".stratadb.lock";
const COMPACTION_SUFFIX: &str = ".wal.tmp";

#[derive(Clone, Debug)]
pub struct Config {
    pub max_segment_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_segment_bytes: DEFAULT_SEGMENT_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValueRef {
    segment_id: u64,
    offset: u64,
    len: u32,
    sequence: u64,
}

#[derive(Debug)]
struct State {
    index: BTreeMap<Vec<u8>, ValueRef>,
    sequence: u64,
    segment_ids: Vec<u64>,
    segment_files: BTreeMap<u64, Arc<File>>,
    recovered_tail_bytes: u64,
}

#[derive(Debug)]
struct SegmentWriter {
    id: u64,
    file: File,
    len: u64,
}

#[derive(Clone, Debug, Default)]
pub struct WriteBatch {
    operations: Vec<Operation>,
}

impl WriteBatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.operations.push(Operation::Put {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> &mut Self {
        self.operations.push(Operation::Delete { key: key.into() });
        self
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    sequence: u64,
    entries: Arc<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl Snapshot {
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.entries.get(key).map(Vec::as_slice)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stats {
    pub sequence: u64,
    pub key_count: usize,
    pub segment_count: usize,
    pub disk_bytes: u64,
    pub recovered_tail_bytes: u64,
}

pub struct Engine {
    directory: PathBuf,
    config: Config,
    _lock: File,
    state: RwLock<State>,
    writer: Mutex<SegmentWriter>,
}

impl Engine {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_config(directory, Config::default())
    }

    pub fn open_with_config(directory: impl AsRef<Path>, config: Config) -> Result<Self> {
        if config.max_segment_bytes < u64::try_from(HEADER_LEN + 10).expect("constant fits u64") {
            return Err(Error::InvalidInput(
                "max_segment_bytes is too small for a record".to_owned(),
            ));
        }
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory)?;
        let lock_path = directory.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        lock.try_lock()
            .map_err(|_| Error::Locked(directory.clone()))?;
        remove_stale_compaction_files(&directory)?;

        let mut segments = list_segments(&directory)?;
        if segments.is_empty() {
            let path = segment_path(&directory, 1);
            let file = OpenOptions::new()
                .create_new(true)
                .read(true)
                .append(true)
                .open(path)?;
            file.sync_all()?;
            segments.push((1, segment_path(&directory, 1)));
        }

        let mut index = BTreeMap::new();
        let mut sequence = 0_u64;
        let mut recovered_tail_bytes = 0_u64;
        for (position, (segment_id, path)) in segments.iter().enumerate() {
            let is_last = position + 1 == segments.len();
            let policy = if is_last {
                TailPolicy::Repair
            } else {
                TailPolicy::Strict
            };
            let report = scan_segment(path, policy)?;
            recovered_tail_bytes += report.discarded_tail_bytes;
            for record in report.records {
                if record.batch.sequence < sequence {
                    return Err(Error::Corruption {
                        path: path.clone(),
                        offset: record.offset,
                        message: format!(
                            "sequence moved backwards from {sequence} to {}",
                            record.batch.sequence
                        ),
                    });
                }
                sequence = sequence.max(record.batch.sequence);
                apply_recovered_record(
                    &mut index,
                    *segment_id,
                    record.offset,
                    &record.batch.operations,
                    &record.batch.value_offsets,
                    record.batch.sequence,
                )?;
            }
        }

        let (active_id, active_path) = segments.last().expect("segments is non-empty");
        let active_file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(active_path)?;
        let active_len = active_file.metadata()?.len();
        let segment_ids = segments.iter().map(|(id, _)| *id).collect();
        let segment_files = segments
            .iter()
            .map(|(id, path)| Ok((*id, Arc::new(File::open(path)?))))
            .collect::<Result<BTreeMap<_, _>>>()?;

        Ok(Self {
            directory,
            config,
            _lock: lock,
            state: RwLock::new(State {
                index,
                sequence,
                segment_ids,
                segment_files,
                recovered_tail_bytes,
            }),
            writer: Mutex::new(SegmentWriter {
                id: *active_id,
                file: active_file,
                len: active_len,
            }),
        })
    }

    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<u64> {
        let mut batch = WriteBatch::new();
        batch.put(key.to_vec(), value.to_vec());
        self.write(&batch)
    }

    pub fn delete(&self, key: &[u8]) -> Result<u64> {
        let mut batch = WriteBatch::new();
        batch.delete(key.to_vec());
        self.write(&batch)
    }

    pub fn write(&self, batch: &WriteBatch) -> Result<u64> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let mut state = self.state.write().expect("state lock poisoned");
        let sequence = state
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::InvalidInput("sequence number exhausted".to_owned()))?;
        let encoded = encode_batch(sequence, &batch.operations)?;
        self.rotate_if_needed(&mut writer, &mut state, encoded.bytes.len())?;
        let record_offset = writer.len;
        writer.file.write_all(&encoded.bytes)?;
        writer.file.sync_data()?;
        writer.len += u64::try_from(encoded.bytes.len()).expect("record length fits u64");
        apply_written_record(
            &mut state.index,
            writer.id,
            record_offset,
            &batch.operations,
            &encoded,
            sequence,
        )?;
        state.sequence = sequence;
        Ok(sequence)
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let state = self.state.read().expect("state lock poisoned");
        state
            .index
            .get(key)
            .copied()
            .map(|reference| read_value(&state.segment_files, reference))
            .transpose()
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let state = self.state.read().expect("state lock poisoned");
        let mut entries = BTreeMap::new();
        for (key, reference) in &state.index {
            entries.insert(key.clone(), read_value(&state.segment_files, *reference)?);
        }
        Ok(Snapshot {
            sequence: state.sequence,
            entries: Arc::new(entries),
        })
    }

    pub fn stats(&self) -> Result<Stats> {
        let state = self.state.read().expect("state lock poisoned");
        let disk_bytes = state.segment_ids.iter().try_fold(0_u64, |total, id| {
            Ok::<u64, Error>(total + fs::metadata(segment_path(&self.directory, *id))?.len())
        })?;
        Ok(Stats {
            sequence: state.sequence,
            key_count: state.index.len(),
            segment_count: state.segment_ids.len(),
            disk_bytes,
            recovered_tail_bytes: state.recovered_tail_bytes,
        })
    }

    pub fn compact(&self) -> Result<Stats> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        let mut state = self.state.write().expect("state lock poisoned");
        let old_segment_ids = state.segment_ids.clone();
        let mut operations = Vec::with_capacity(state.index.len());
        for (key, reference) in &state.index {
            operations.push(Operation::Put {
                key: key.clone(),
                value: read_value(&state.segment_files, *reference)?,
            });
        }

        let first_new_id = old_segment_ids
            .last()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| Error::InvalidInput("segment id exhausted".to_owned()))?;
        let chunks = compaction_chunks(operations)?;
        let mut new_segment_ids = Vec::new();
        let mut new_index = BTreeMap::new();

        if chunks.is_empty() {
            let temporary = temporary_segment_path(&self.directory, first_new_id);
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, segment_path(&self.directory, first_new_id))?;
            new_segment_ids.push(first_new_id);
        } else {
            for (chunk_number, chunk) in chunks.iter().enumerate() {
                let id = first_new_id
                    .checked_add(u64::try_from(chunk_number).expect("chunk count fits u64"))
                    .ok_or_else(|| Error::InvalidInput("segment id exhausted".to_owned()))?;
                let encoded = encode_batch(state.sequence, chunk)?;
                let temporary = temporary_segment_path(&self.directory, id);
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temporary)?;
                file.write_all(&encoded.bytes)?;
                file.sync_all()?;
                drop(file);
                fs::rename(&temporary, segment_path(&self.directory, id))?;
                apply_written_record(&mut new_index, id, 0, chunk, &encoded, state.sequence)?;
                new_segment_ids.push(id);
            }
        }

        let active_id = *new_segment_ids.last().expect("compaction emits a segment");
        let active_path = segment_path(&self.directory, active_id);
        let active_file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&active_path)?;
        let active_len = active_file.metadata()?.len();
        let new_segment_files = new_segment_ids
            .iter()
            .map(|id| {
                Ok((
                    *id,
                    Arc::new(File::open(segment_path(&self.directory, *id))?),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        *writer = SegmentWriter {
            id: active_id,
            file: active_file,
            len: active_len,
        };
        state.index = new_index;
        state.segment_ids.clone_from(&new_segment_ids);
        state.segment_files = new_segment_files;

        for id in old_segment_ids {
            let path = segment_path(&self.directory, id);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        drop(state);
        drop(writer);
        self.stats()
    }

    fn rotate_if_needed(
        &self,
        writer: &mut SegmentWriter,
        state: &mut State,
        record_len: usize,
    ) -> Result<()> {
        let record_len = u64::try_from(record_len).expect("record length fits u64");
        if writer.len == 0 || writer.len.saturating_add(record_len) <= self.config.max_segment_bytes
        {
            return Ok(());
        }
        writer.file.sync_all()?;
        let next_id = writer
            .id
            .checked_add(1)
            .ok_or_else(|| Error::InvalidInput("segment id exhausted".to_owned()))?;
        let path = segment_path(&self.directory, next_id);
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .append(true)
            .open(path)?;
        file.sync_all()?;
        let read_file = Arc::new(File::open(segment_path(&self.directory, next_id))?);
        *writer = SegmentWriter {
            id: next_id,
            file,
            len: 0,
        };
        state.segment_ids.push(next_id);
        state.segment_files.insert(next_id, read_file);
        Ok(())
    }
}

fn apply_recovered_record(
    index: &mut BTreeMap<Vec<u8>, ValueRef>,
    segment_id: u64,
    record_offset: u64,
    operations: &[Operation],
    value_offsets: &[Option<(u32, u32)>],
    sequence: u64,
) -> Result<()> {
    if operations.len() != value_offsets.len() {
        return Err(Error::InvalidInput(
            "operation/value offset count mismatch".to_owned(),
        ));
    }
    for (operation, value_offset) in operations.iter().zip(value_offsets) {
        match (operation, value_offset) {
            (Operation::Put { key, .. }, Some((offset, len))) => {
                index.insert(
                    key.clone(),
                    ValueRef {
                        segment_id,
                        offset: record_offset
                            + u64::try_from(HEADER_LEN).expect("constant fits u64")
                            + u64::from(*offset),
                        len: *len,
                        sequence,
                    },
                );
            }
            (Operation::Delete { key }, None) => {
                index.remove(key);
            }
            _ => {
                return Err(Error::InvalidInput(
                    "operation/value offset type mismatch".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn apply_written_record(
    index: &mut BTreeMap<Vec<u8>, ValueRef>,
    segment_id: u64,
    record_offset: u64,
    operations: &[Operation],
    encoded: &EncodedBatch,
    sequence: u64,
) -> Result<()> {
    apply_recovered_record(
        index,
        segment_id,
        record_offset,
        operations,
        &encoded.value_offsets,
        sequence,
    )
}

fn read_value(segment_files: &BTreeMap<u64, Arc<File>>, reference: ValueRef) -> Result<Vec<u8>> {
    let file = segment_files.get(&reference.segment_id).ok_or_else(|| {
        Error::InvalidInput(format!(
            "missing file handle for segment {}",
            reference.segment_id
        ))
    })?;
    let mut value = vec![0_u8; usize::try_from(reference.len).expect("u32 fits usize")];
    read_exact_at(file, &mut value, reference.offset)?;
    Ok(value)
}

#[cfg(unix)]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<()> {
    let mut read = 0_usize;
    while read < buffer.len() {
        let count = file.read_at(
            &mut buffer[read..],
            offset + u64::try_from(read).expect("buffer offset fits u64"),
        )?;
        if count == 0 {
            return Err(Error::Io(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        read += count;
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<()> {
    let mut read = 0_usize;
    while read < buffer.len() {
        let count = file.seek_read(
            &mut buffer[read..],
            offset + u64::try_from(read).expect("buffer offset fits u64"),
        )?;
        if count == 0 {
            return Err(Error::Io(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        read += count;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<()> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buffer)?;
    Ok(())
}

fn compaction_chunks(operations: Vec<Operation>) -> Result<Vec<Vec<Operation>>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut encoded_bytes = HEADER_LEN;
    for operation in operations {
        let operation_bytes = 9
            + operation.key().len()
            + match &operation {
                Operation::Put { value, .. } => value.len(),
                Operation::Delete { .. } => 0,
            };
        if !chunk.is_empty()
            && (encoded_bytes + operation_bytes > MAX_RECORD_BYTES
                || chunk.len() == MAX_BATCH_OPERATIONS)
        {
            chunks.push(std::mem::take(&mut chunk));
            encoded_bytes = HEADER_LEN;
        }
        encoded_bytes = encoded_bytes
            .checked_add(operation_bytes)
            .ok_or_else(|| Error::InvalidInput("compaction size overflow".to_owned()))?;
        chunk.push(operation);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    Ok(chunks)
}

fn temporary_segment_path(directory: &Path, id: u64) -> PathBuf {
    directory.join(format!("segment-{id:020}{COMPACTION_SUFFIX}"))
}

fn remove_stale_compaction_files(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("segment-") && name.ends_with(COMPACTION_SUFFIX) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{Config, Engine, WriteBatch};

    fn test_directory(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "stratadb-engine-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn put_delete_batch_snapshot_and_reopen() {
        let directory = test_directory("roundtrip");
        {
            let engine = Engine::open(&directory).unwrap();
            engine.put(b"alpha", b"one").unwrap();
            let mut batch = WriteBatch::new();
            batch
                .put(b"alpha".to_vec(), b"two".to_vec())
                .put(b"beta".to_vec(), b"three".to_vec());
            let sequence = engine.write(&batch).unwrap();
            let snapshot = engine.snapshot().unwrap();
            engine.delete(b"beta").unwrap();
            assert_eq!(snapshot.sequence(), sequence);
            assert_eq!(snapshot.get(b"alpha"), Some(b"two".as_slice()));
            assert_eq!(snapshot.get(b"beta"), Some(b"three".as_slice()));
            assert_eq!(engine.get(b"beta").unwrap(), None);
        }
        {
            let engine = Engine::open(&directory).unwrap();
            assert_eq!(engine.get(b"alpha").unwrap(), Some(b"two".to_vec()));
            assert_eq!(engine.get(b"beta").unwrap(), None);
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rotation_and_compaction_preserve_latest_values() {
        let directory = test_directory("compact");
        {
            let engine = Engine::open_with_config(
                &directory,
                Config {
                    max_segment_bytes: 96,
                },
            )
            .unwrap();
            for version in 0..20 {
                engine
                    .put(b"key", format!("value-{version:02}").as_bytes())
                    .unwrap();
            }
            assert!(engine.stats().unwrap().segment_count > 1);
            let stats = engine.compact().unwrap();
            assert_eq!(stats.segment_count, 1);
            assert_eq!(engine.get(b"key").unwrap(), Some(b"value-19".to_vec()));
        }
        {
            let engine = Engine::open(&directory).unwrap();
            assert_eq!(engine.get(b"key").unwrap(), Some(b"value-19".to_vec()));
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
