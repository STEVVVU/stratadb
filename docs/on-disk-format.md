# On-Disk Format

All integers are unsigned little-endian. Segment files are named
`segment-{id:020}.wal`; IDs increase monotonically. Temporary compaction files
end in `.wal.tmp` and are never replayed.

## Record header

Each WAL record consists of a fixed 28-byte header followed by one batch
payload.

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 4 | Magic ASCII `SKV1` |
| 4 | 1 | Format version (`1`) |
| 5 | 1 | Record kind (`1` = batch) |
| 6 | 2 | Reserved flags, must be zero |
| 8 | 4 | Payload length |
| 12 | 8 | Logical sequence |
| 20 | 4 | Operation count |
| 24 | 4 | CRC-32 |
| 28 | N | Batch payload |

The CRC is standard CRC-32/ISO-HDLC using reversed polynomial `0xEDB88320`,
initial value `0xFFFFFFFF`, and final XOR `0xFFFFFFFF`. It covers header bytes
`4..24` followed by the complete payload. Magic and the stored checksum are
not included.

This parameterization is the format contract and is independent of which
implementation computes it. `crc32::checksum` (backed by `crc32fast`, hardware
accelerated) is used at runtime; `crc32::reference` is the original bitwise
implementation, retained as executable documentation of the definition above. A
differential test asserts the two agree bit-for-bit, and separate known-answer
vectors generated with Python's `zlib.crc32` pin both to this specification.
Segments written by either implementation are byte-identical and interchangeable.

## Operation encoding

Operations are concatenated without padding:

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 1 | Tag (`1` = put, `2` = delete) |
| 1 | 4 | Key length |
| 5 | 4 | Value length |
| 9 | K | Key bytes |
| 9+K | V | Value bytes |

Delete records require value length zero. Keys and values are opaque bytes;
there is no text encoding requirement.

## Hard bounds

| Item | Maximum |
| --- | ---: |
| Key | 65,536 bytes |
| Value | 4,194,304 bytes |
| Encoded record including header | 16,777,216 bytes |
| Operations in one batch | 10,000 |

Bounds are checked before append and again while decoding. Payload lengths are
validated before allocation, and every operation must consume exactly the
declared payload; trailing bytes invalidate the record.

## Atomicity

One batch is one record and one checksum domain. Replay applies a record only
after reading its complete declared payload and validating every operation.
A partial record therefore contributes no operations.

Sequences increase for client writes. Compaction may emit multiple records
with the same sequence because it rewrites one logical snapshot; recovery
accepts non-decreasing sequences and rejects backwards movement.

## Compatibility

Readers reject unknown versions, record kinds, non-zero flags, and unknown
operation tags. There is no in-place upgrade path in version 1. A future
format migration must write new higher-ID segments or use an explicit export
and import tool.
