use crate::crc32::checksum;
use crate::{Error, Result};

pub const MAGIC: [u8; 4] = *b"SKV1";
pub const FORMAT_VERSION: u8 = 1;
pub const RECORD_KIND_BATCH: u8 = 1;
pub const HEADER_LEN: usize = 28;
pub const MAX_KEY_BYTES: usize = 64 * 1024;
pub const MAX_VALUE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BATCH_OPERATIONS: usize = 10_000;

const OP_PUT: u8 = 1;
const OP_DELETE: u8 = 2;
const OP_HEADER_LEN: usize = 9;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl Operation {
    #[must_use]
    pub fn key(&self) -> &[u8] {
        match self {
            Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedBatch {
    pub sequence: u64,
    pub operations: Vec<Operation>,
    /// Relative offsets from the start of the payload to each put value.
    pub value_offsets: Vec<Option<(u32, u32)>>,
}

#[derive(Debug)]
pub struct EncodedBatch {
    pub bytes: Vec<u8>,
    pub value_offsets: Vec<Option<(u32, u32)>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub payload_len: u32,
    pub sequence: u64,
    pub operation_count: u32,
    pub expected_checksum: u32,
}

pub fn encode_batch(sequence: u64, operations: &[Operation]) -> Result<EncodedBatch> {
    validate_operations(operations)?;
    let payload_capacity = operations.iter().try_fold(0_usize, |total, operation| {
        let value_len = match operation {
            Operation::Put { value, .. } => value.len(),
            Operation::Delete { .. } => 0,
        };
        total
            .checked_add(OP_HEADER_LEN)
            .and_then(|size| size.checked_add(operation.key().len()))
            .and_then(|size| size.checked_add(value_len))
            .ok_or_else(|| Error::InvalidInput("batch size overflow".to_owned()))
    })?;
    if payload_capacity + HEADER_LEN > MAX_RECORD_BYTES {
        return Err(Error::InvalidInput(format!(
            "encoded record exceeds {MAX_RECORD_BYTES} bytes"
        )));
    }

    let mut payload = Vec::with_capacity(payload_capacity);
    let mut value_offsets = Vec::with_capacity(operations.len());
    for operation in operations {
        match operation {
            Operation::Put { key, value } => {
                payload.push(OP_PUT);
                put_u32(&mut payload, key.len())?;
                put_u32(&mut payload, value.len())?;
                payload.extend_from_slice(key);
                let value_offset = u32::try_from(payload.len())
                    .map_err(|_| Error::InvalidInput("value offset overflow".to_owned()))?;
                payload.extend_from_slice(value);
                value_offsets.push(Some((
                    value_offset,
                    u32::try_from(value.len())
                        .map_err(|_| Error::InvalidInput("value length overflow".to_owned()))?,
                )));
            }
            Operation::Delete { key } => {
                payload.push(OP_DELETE);
                put_u32(&mut payload, key.len())?;
                put_u32(&mut payload, 0)?;
                payload.extend_from_slice(key);
                value_offsets.push(None);
            }
        }
    }

    let payload_len = u32::try_from(payload.len())
        .map_err(|_| Error::InvalidInput("payload length overflow".to_owned()))?;
    let operation_count = u32::try_from(operations.len())
        .map_err(|_| Error::InvalidInput("operation count overflow".to_owned()))?;
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(&MAGIC);
    header.push(FORMAT_VERSION);
    header.push(RECORD_KIND_BATCH);
    header.extend_from_slice(&0_u16.to_le_bytes());
    header.extend_from_slice(&payload_len.to_le_bytes());
    header.extend_from_slice(&sequence.to_le_bytes());
    header.extend_from_slice(&operation_count.to_le_bytes());
    let crc = checksum(&[&header[4..], &payload]);
    header.extend_from_slice(&crc.to_le_bytes());
    debug_assert_eq!(header.len(), HEADER_LEN);
    header.extend_from_slice(&payload);

    Ok(EncodedBatch {
        bytes: header,
        value_offsets,
    })
}

pub fn decode_header(bytes: &[u8; HEADER_LEN]) -> Result<Header> {
    if bytes[..4] != MAGIC {
        return Err(Error::InvalidInput("record magic mismatch".to_owned()));
    }
    if bytes[4] != FORMAT_VERSION {
        return Err(Error::InvalidInput(format!(
            "unsupported format version {}",
            bytes[4]
        )));
    }
    if bytes[5] != RECORD_KIND_BATCH {
        return Err(Error::InvalidInput(format!(
            "unsupported record kind {}",
            bytes[5]
        )));
    }
    if u16::from_le_bytes(bytes[6..8].try_into().expect("fixed slice")) != 0 {
        return Err(Error::InvalidInput("non-zero reserved flags".to_owned()));
    }
    let payload_len = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed slice"));
    if usize::try_from(payload_len).expect("u32 fits usize") + HEADER_LEN > MAX_RECORD_BYTES {
        return Err(Error::InvalidInput(format!(
            "record exceeds {MAX_RECORD_BYTES} bytes"
        )));
    }
    let operation_count = u32::from_le_bytes(bytes[20..24].try_into().expect("fixed slice"));
    if usize::try_from(operation_count).expect("u32 fits usize") > MAX_BATCH_OPERATIONS {
        return Err(Error::InvalidInput(format!(
            "batch exceeds {MAX_BATCH_OPERATIONS} operations"
        )));
    }
    Ok(Header {
        payload_len,
        sequence: u64::from_le_bytes(bytes[12..20].try_into().expect("fixed slice")),
        operation_count,
        expected_checksum: u32::from_le_bytes(bytes[24..28].try_into().expect("fixed slice")),
    })
}

pub fn decode_payload(header_bytes: &[u8; HEADER_LEN], payload: &[u8]) -> Result<DecodedBatch> {
    let header = decode_header(header_bytes)?;
    if payload.len() != usize::try_from(header.payload_len).expect("u32 fits usize") {
        return Err(Error::InvalidInput("payload length mismatch".to_owned()));
    }
    let actual_checksum = checksum(&[&header_bytes[4..24], payload]);
    if actual_checksum != header.expected_checksum {
        return Err(Error::InvalidInput(format!(
            "checksum mismatch: expected {:08x}, got {actual_checksum:08x}",
            header.expected_checksum
        )));
    }

    let mut cursor = 0_usize;
    let mut operations =
        Vec::with_capacity(usize::try_from(header.operation_count).expect("u32 fits usize"));
    let mut value_offsets = Vec::with_capacity(operations.capacity());
    for _ in 0..header.operation_count {
        let operation_header = payload
            .get(cursor..cursor + OP_HEADER_LEN)
            .ok_or_else(|| Error::InvalidInput("truncated operation header".to_owned()))?;
        let tag = operation_header[0];
        let key_len = usize::try_from(u32::from_le_bytes(
            operation_header[1..5].try_into().expect("fixed slice"),
        ))
        .expect("u32 fits usize");
        let value_len = usize::try_from(u32::from_le_bytes(
            operation_header[5..9].try_into().expect("fixed slice"),
        ))
        .expect("u32 fits usize");
        cursor += OP_HEADER_LEN;
        if key_len == 0 || key_len > MAX_KEY_BYTES {
            return Err(Error::InvalidInput("invalid key length".to_owned()));
        }
        if value_len > MAX_VALUE_BYTES {
            return Err(Error::InvalidInput("invalid value length".to_owned()));
        }
        let key_end = cursor
            .checked_add(key_len)
            .ok_or_else(|| Error::InvalidInput("key offset overflow".to_owned()))?;
        let key = payload
            .get(cursor..key_end)
            .ok_or_else(|| Error::InvalidInput("truncated key".to_owned()))?
            .to_vec();
        cursor = key_end;

        match tag {
            OP_PUT => {
                let value_end = cursor
                    .checked_add(value_len)
                    .ok_or_else(|| Error::InvalidInput("value offset overflow".to_owned()))?;
                let value = payload
                    .get(cursor..value_end)
                    .ok_or_else(|| Error::InvalidInput("truncated value".to_owned()))?
                    .to_vec();
                value_offsets.push(Some((
                    u32::try_from(cursor)
                        .map_err(|_| Error::InvalidInput("value offset overflow".to_owned()))?,
                    u32::try_from(value_len)
                        .map_err(|_| Error::InvalidInput("value length overflow".to_owned()))?,
                )));
                operations.push(Operation::Put { key, value });
                cursor = value_end;
            }
            OP_DELETE if value_len == 0 => {
                operations.push(Operation::Delete { key });
                value_offsets.push(None);
            }
            OP_DELETE => {
                return Err(Error::InvalidInput(
                    "delete operation has a value".to_owned(),
                ));
            }
            _ => return Err(Error::InvalidInput(format!("unknown operation tag {tag}"))),
        }
    }
    if cursor != payload.len() {
        return Err(Error::InvalidInput(
            "trailing bytes after batch payload".to_owned(),
        ));
    }
    Ok(DecodedBatch {
        sequence: header.sequence,
        operations,
        value_offsets,
    })
}

fn validate_operations(operations: &[Operation]) -> Result<()> {
    if operations.is_empty() {
        return Err(Error::InvalidInput("batch must not be empty".to_owned()));
    }
    if operations.len() > MAX_BATCH_OPERATIONS {
        return Err(Error::InvalidInput(format!(
            "batch exceeds {MAX_BATCH_OPERATIONS} operations"
        )));
    }
    for operation in operations {
        if operation.key().is_empty() {
            return Err(Error::InvalidInput("key must not be empty".to_owned()));
        }
        if operation.key().len() > MAX_KEY_BYTES {
            return Err(Error::InvalidInput(format!(
                "key exceeds {MAX_KEY_BYTES} bytes"
            )));
        }
        if let Operation::Put { value, .. } = operation {
            if value.len() > MAX_VALUE_BYTES {
                return Err(Error::InvalidInput(format!(
                    "value exceeds {MAX_VALUE_BYTES} bytes"
                )));
            }
        }
    }
    Ok(())
}

fn put_u32(destination: &mut Vec<u8>, value: usize) -> Result<()> {
    let value = u32::try_from(value)
        .map_err(|_| Error::InvalidInput("length does not fit u32".to_owned()))?;
    destination.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        decode_payload, encode_batch, Operation, HEADER_LEN, MAX_KEY_BYTES, MAX_VALUE_BYTES,
    };

    #[test]
    fn batch_round_trip_preserves_binary_keys_and_values() {
        let operations = vec![
            Operation::Put {
                key: vec![0, 1, 2],
                value: vec![255, 0, 7],
            },
            Operation::Delete {
                key: b"removed".to_vec(),
            },
        ];
        let encoded = encode_batch(42, &operations).unwrap();
        let header: &[u8; HEADER_LEN] = encoded.bytes[..HEADER_LEN].try_into().unwrap();
        let decoded = decode_payload(header, &encoded.bytes[HEADER_LEN..]).unwrap();
        assert_eq!(decoded.sequence, 42);
        assert_eq!(decoded.operations, operations);
        assert_eq!(decoded.value_offsets, encoded.value_offsets);
    }

    #[test]
    fn checksum_detects_payload_mutation() {
        let mut encoded = encode_batch(
            1,
            &[Operation::Put {
                key: b"k".to_vec(),
                value: b"value".to_vec(),
            }],
        )
        .unwrap();
        *encoded.bytes.last_mut().unwrap() ^= 0xff;
        let header: &[u8; HEADER_LEN] = encoded.bytes[..HEADER_LEN].try_into().unwrap();
        assert!(decode_payload(header, &encoded.bytes[HEADER_LEN..]).is_err());
    }

    #[test]
    fn bounds_are_enforced_before_allocation_to_disk() {
        assert!(encode_batch(
            1,
            &[Operation::Put {
                key: vec![b'k'; MAX_KEY_BYTES + 1],
                value: vec![],
            }]
        )
        .is_err());
        assert!(encode_batch(
            1,
            &[Operation::Put {
                key: b"k".to_vec(),
                value: vec![0; MAX_VALUE_BYTES + 1],
            }]
        )
        .is_err());
    }
}
