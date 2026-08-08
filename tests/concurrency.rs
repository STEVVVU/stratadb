mod common;

use std::sync::{Arc, Barrier};
use std::thread;

use common::TestDirectory;
use stratadb::{Engine, WriteBatch};

#[test]
fn concurrent_readers_observe_only_complete_batches() {
    let directory = TestDirectory::new("concurrent-readers");
    let engine = Arc::new(Engine::open(directory.path()).unwrap());
    let mut initial = WriteBatch::new();
    initial
        .put(b"left".to_vec(), 0_u64.to_le_bytes().to_vec())
        .put(b"right".to_vec(), 0_u64.to_le_bytes().to_vec())
        .put(b"constant".to_vec(), b"present".to_vec());
    engine.write(&initial).unwrap();

    let reader_count = 8;
    let barrier = Arc::new(Barrier::new(reader_count + 1));
    let mut readers = Vec::new();
    for _ in 0..reader_count {
        let engine = Arc::clone(&engine);
        let barrier = Arc::clone(&barrier);
        readers.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..200 {
                assert_eq!(engine.get(b"constant").unwrap(), Some(b"present".to_vec()));
                let snapshot = engine.snapshot().unwrap();
                assert_eq!(snapshot.get(b"left"), snapshot.get(b"right"));
            }
        }));
    }

    barrier.wait();
    for generation in 1_u64..=200 {
        let value = generation.to_le_bytes().to_vec();
        let mut batch = WriteBatch::new();
        batch
            .put(b"left".to_vec(), value.clone())
            .put(b"right".to_vec(), value);
        engine.write(&batch).unwrap();
    }
    for reader in readers {
        reader.join().unwrap();
    }
    let final_snapshot = engine.snapshot().unwrap();
    assert_eq!(
        final_snapshot.get(b"left"),
        Some(200_u64.to_le_bytes().as_slice())
    );
    assert_eq!(final_snapshot.get(b"left"), final_snapshot.get(b"right"));
}

#[test]
fn snapshot_remains_stable_after_later_writes() {
    let directory = TestDirectory::new("snapshot-stability");
    let engine = Engine::open(directory.path()).unwrap();
    engine.put(b"key", b"old").unwrap();
    let snapshot = engine.snapshot().unwrap();
    engine.put(b"key", b"new").unwrap();
    engine.put(b"other", b"value").unwrap();

    assert_eq!(snapshot.get(b"key"), Some(b"old".as_slice()));
    assert_eq!(snapshot.get(b"other"), None);
    assert_eq!(engine.get(b"key").unwrap(), Some(b"new".to_vec()));
}
