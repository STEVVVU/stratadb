mod common;

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use common::TestDirectory;
use stratadb::format::{encode_batch, Operation};
use stratadb::wal::{list_segments, segment_path};
use stratadb::{Config, Engine, Error, WriteBatch};

#[test]
fn torn_atomic_batch_is_discarded_without_partial_visibility() {
    let directory = TestDirectory::new("torn-batch");
    let next_sequence;
    let active_path;
    {
        let engine = Engine::open(directory.path()).unwrap();
        engine.put(b"stable", b"before").unwrap();
        next_sequence = engine.stats().unwrap().sequence + 1;
        active_path = list_segments(directory.path()).unwrap().pop().unwrap().1;
    }

    let encoded = encode_batch(
        next_sequence,
        &[
            Operation::Put {
                key: b"first".to_vec(),
                value: b"must-not-appear".to_vec(),
            },
            Operation::Put {
                key: b"second".to_vec(),
                value: b"must-not-appear".to_vec(),
            },
        ],
    )
    .unwrap();
    let torn_at = encoded.bytes.len() - 5;
    let mut file = OpenOptions::new().append(true).open(active_path).unwrap();
    file.write_all(&encoded.bytes[..torn_at]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let engine = Engine::open(directory.path()).unwrap();
    assert_eq!(engine.get(b"stable").unwrap(), Some(b"before".to_vec()));
    assert_eq!(engine.get(b"first").unwrap(), None);
    assert_eq!(engine.get(b"second").unwrap(), None);
    assert_eq!(engine.stats().unwrap().recovered_tail_bytes, torn_at as u64);
}

#[test]
fn corrupt_checksum_in_active_tail_is_truncated() {
    let directory = TestDirectory::new("bad-tail-crc");
    let next_sequence;
    let active_path;
    {
        let engine = Engine::open(directory.path()).unwrap();
        engine.put(b"stable", b"value").unwrap();
        next_sequence = engine.stats().unwrap().sequence + 1;
        active_path = list_segments(directory.path()).unwrap().pop().unwrap().1;
    }
    let mut encoded = encode_batch(
        next_sequence,
        &[Operation::Put {
            key: b"corrupt".to_vec(),
            value: b"value".to_vec(),
        }],
    )
    .unwrap();
    *encoded.bytes.last_mut().unwrap() ^= 0x80;
    let mut file = OpenOptions::new().append(true).open(active_path).unwrap();
    file.write_all(&encoded.bytes).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let engine = Engine::open(directory.path()).unwrap();
    assert_eq!(engine.get(b"stable").unwrap(), Some(b"value".to_vec()));
    assert_eq!(engine.get(b"corrupt").unwrap(), None);
    assert_eq!(
        engine.stats().unwrap().recovered_tail_bytes,
        encoded.bytes.len() as u64
    );
}

#[test]
fn corruption_in_sealed_segment_fails_open() {
    let directory = TestDirectory::new("sealed-corruption");
    {
        let engine = Engine::open_with_config(
            directory.path(),
            Config {
                max_segment_bytes: 80,
            },
        )
        .unwrap();
        for number in 0..4 {
            engine
                .put(
                    format!("key-{number}").as_bytes(),
                    format!("value-{number}-xxxxxxxx").as_bytes(),
                )
                .unwrap();
        }
        assert!(engine.stats().unwrap().segment_count > 1);
    }
    let first_path = segment_path(directory.path(), 1);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(first_path)
        .unwrap();
    let last_offset = file.metadata().unwrap().len() - 1;
    file.seek(SeekFrom::Start(last_offset)).unwrap();
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0x01;
    file.seek(SeekFrom::Start(last_offset)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
    drop(file);

    assert!(matches!(
        Engine::open(directory.path()),
        Err(Error::Corruption { .. })
    ));
}

#[test]
fn exclusive_lock_rejects_second_writer() {
    let directory = TestDirectory::new("writer-lock");
    let first = Engine::open(directory.path()).unwrap();
    assert!(matches!(
        Engine::open(directory.path()),
        Err(Error::Locked(_))
    ));
    drop(first);
    Engine::open(directory.path()).unwrap();
}

#[test]
fn committed_batch_recovers_as_one_sequence() {
    let directory = TestDirectory::new("committed-batch");
    let sequence;
    {
        let engine = Engine::open(directory.path()).unwrap();
        let mut batch = WriteBatch::new();
        batch
            .put(b"a".to_vec(), b"one".to_vec())
            .put(b"b".to_vec(), b"two".to_vec())
            .delete(b"absent".to_vec());
        sequence = engine.write(&batch).unwrap();
    }
    let engine = Engine::open(directory.path()).unwrap();
    assert_eq!(engine.stats().unwrap().sequence, sequence);
    assert_eq!(engine.get(b"a").unwrap(), Some(b"one".to_vec()));
    assert_eq!(engine.get(b"b").unwrap(), Some(b"two".to_vec()));
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
}
