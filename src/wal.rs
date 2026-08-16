use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::format::{decode_header, decode_payload, DecodedBatch, HEADER_LEN};
use crate::{Error, Result};

pub const SEGMENT_PREFIX: &str = "segment-";
pub const SEGMENT_SUFFIX: &str = ".wal";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TailPolicy {
    Strict,
    Repair,
}

#[derive(Clone, Debug)]
pub struct ScannedRecord {
    pub offset: u64,
    pub byte_len: u64,
    pub batch: DecodedBatch,
}

#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    pub records: Vec<ScannedRecord>,
    pub valid_bytes: u64,
    pub discarded_tail_bytes: u64,
}

#[must_use]
pub fn segment_name(id: u64) -> String {
    format!("{SEGMENT_PREFIX}{id:020}{SEGMENT_SUFFIX}")
}

#[must_use]
pub fn segment_path(directory: &Path, id: u64) -> PathBuf {
    directory.join(segment_name(id))
}

pub fn list_segments(directory: &Path) -> Result<Vec<(u64, PathBuf)>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(number) = name
            .strip_prefix(SEGMENT_PREFIX)
            .and_then(|rest| rest.strip_suffix(SEGMENT_SUFFIX))
        else {
            continue;
        };
        if number.len() != 20 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let id = number.parse::<u64>().map_err(|error| {
            Error::InvalidInput(format!("invalid segment name {name}: {error}"))
        })?;
        segments.push((id, entry.path()));
    }
    segments.sort_unstable_by_key(|(id, _)| *id);
    Ok(segments)
}

pub fn scan_segment(path: &Path, policy: TailPolicy) -> Result<ScanReport> {
    let mut options = OpenOptions::new();
    options.read(true);
    if policy == TailPolicy::Repair {
        options.write(true);
    }
    let mut file = options.open(path)?;
    let original_len = file.metadata()?.len();
    let mut report = ScanReport::default();

    loop {
        let record_offset = file.stream_position()?;
        let mut header = [0_u8; HEADER_LEN];
        let header_bytes = read_up_to(&mut file, &mut header)?;
        if header_bytes == 0 {
            report.valid_bytes = record_offset;
            return Ok(report);
        }
        if header_bytes < HEADER_LEN {
            return tail_problem(
                path,
                &mut file,
                policy,
                &mut report,
                record_offset,
                original_len,
                format!("torn header: read {header_bytes} of {HEADER_LEN} bytes"),
            );
        }

        let decoded_header = match decode_header(&header) {
            Ok(header) => header,
            Err(error) => {
                return tail_problem(
                    path,
                    &mut file,
                    policy,
                    &mut report,
                    record_offset,
                    original_len,
                    error.to_string(),
                );
            }
        };
        let payload_len = usize::try_from(decoded_header.payload_len).expect("u32 fits usize");
        let mut payload = vec![0_u8; payload_len];
        let payload_bytes = read_up_to(&mut file, &mut payload)?;
        if payload_bytes < payload_len {
            return tail_problem(
                path,
                &mut file,
                policy,
                &mut report,
                record_offset,
                original_len,
                format!("torn payload: read {payload_bytes} of {payload_len} bytes"),
            );
        }
        let batch = match decode_payload(&header, &payload) {
            Ok(batch) => batch,
            Err(error) => {
                return tail_problem(
                    path,
                    &mut file,
                    policy,
                    &mut report,
                    record_offset,
                    original_len,
                    error.to_string(),
                );
            }
        };
        let byte_len = u64::try_from(HEADER_LEN + payload_len).expect("record length fits u64");
        report.records.push(ScannedRecord {
            offset: record_offset,
            byte_len,
            batch,
        });
        report.valid_bytes = record_offset + byte_len;
    }
}

fn read_up_to(file: &mut File, buffer: &mut [u8]) -> Result<usize> {
    let mut read = 0;
    while read < buffer.len() {
        match file.read(&mut buffer[read..])? {
            0 => break,
            count => read += count,
        }
    }
    Ok(read)
}

fn tail_problem(
    path: &Path,
    file: &mut File,
    policy: TailPolicy,
    report: &mut ScanReport,
    valid_end: u64,
    original_len: u64,
    message: String,
) -> Result<ScanReport> {
    if policy == TailPolicy::Strict {
        return Err(Error::Corruption {
            path: path.to_path_buf(),
            offset: valid_end,
            message,
        });
    }
    file.set_len(valid_end)?;
    file.seek(SeekFrom::Start(valid_end))?;
    file.sync_data()?;
    report.valid_bytes = valid_end;
    report.discarded_tail_bytes = original_len.saturating_sub(valid_end);
    Ok(std::mem::take(report))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    use super::{scan_segment, TailPolicy};
    use crate::format::{encode_batch, Operation};

    #[test]
    fn repair_discards_only_incomplete_tail() {
        let directory = tempfile::Builder::new()
            .prefix("stratadb-format-")
            .tempdir()
            .unwrap();
        let path = directory.path().join("segment.wal");
        let record = encode_batch(
            1,
            &[Operation::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }],
        )
        .unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all(&record.bytes).unwrap();
        file.write_all(b"SKV").unwrap();
        file.sync_all().unwrap();
        drop(file);

        assert!(scan_segment(&path, TailPolicy::Strict).is_err());
        let report = scan_segment(&path, TailPolicy::Repair).unwrap();
        assert_eq!(report.records.len(), 1);
        assert_eq!(report.discarded_tail_bytes, 3);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            record.bytes.len() as u64
        );
    }
}
