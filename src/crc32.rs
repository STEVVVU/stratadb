//! CRC-32/ISO-HDLC over the WAL record checksum domain.
//!
//! Two implementations live here on purpose:
//!
//! - [`checksum`] is the production path. It delegates to `crc32fast`, which
//!   dispatches at runtime to a carry-less-multiply implementation (SSE4.2 /
//!   PCLMULQDQ on x86-64, PMULL on aarch64) and falls back to a table-driven
//!   scalar path elsewhere. Every byte written to or read from a segment passes
//!   through this function, so it sits directly on the write and recovery hot
//!   paths.
//! - [`reference`] is the original hand-written bitwise implementation. It is
//!   retained as executable documentation of the on-disk contract described in
//!   `docs/on-disk-format.md`: reversed polynomial `0xEDB88320`, initial value
//!   `0xFFFFFFFF`, final XOR `0xFFFFFFFF`. It is not used at runtime.
//!
//! The two are held to bit-for-bit equality by
//! [`tests::accelerated_and_reference_agree_across_corpus`], so swapping the
//! implementation cannot silently change the format. Both are fed the parts in
//! sequence, which is equivalent to hashing their concatenation.

/// Computes the CRC-32 of `parts` treated as one contiguous byte sequence.
///
/// This is the accelerated production implementation. It is byte-for-byte
/// equivalent to [`reference::checksum`].
pub fn checksum(parts: &[&[u8]]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize()
}

/// Reference implementation of CRC-32/ISO-HDLC, kept for documentation and
/// differential testing.
///
/// This is the original dependency-free implementation the on-disk format was
/// specified against. It is deliberately the naive bitwise form: it processes
/// one bit at a time and does not use a lookup table, so it reads as a direct
/// transcription of the polynomial division that defines the checksum. Prefer
/// [`super::checksum`] for anything on a hot path.
pub mod reference {
    const POLYNOMIAL: u32 = 0xedb8_8320;

    /// Computes the CRC-32 of `parts` treated as one contiguous byte sequence.
    pub fn checksum(parts: &[&[u8]]) -> u32 {
        let mut crc = u32::MAX;
        for part in parts {
            for byte in *part {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    let mask = 0_u32.wrapping_sub(crc & 1);
                    crc = (crc >> 1) ^ (POLYNOMIAL & mask);
                }
            }
        }
        !crc
    }
}

#[cfg(test)]
mod tests {
    use super::{checksum, reference};

    #[test]
    fn matches_standard_crc32_vector() {
        assert_eq!(checksum(&[b"123456789"]), 0xcbf4_3926);
        assert_eq!(checksum(&[b"123", b"456", b"789"]), 0xcbf4_3926);
        assert_eq!(reference::checksum(&[b"123456789"]), 0xcbf4_3926);
        assert_eq!(reference::checksum(&[b"123", b"456", b"789"]), 0xcbf4_3926);
    }

    #[test]
    fn known_vectors_pin_the_on_disk_contract() {
        // CRC-32/ISO-HDLC vectors generated externally with Python's
        // `zlib.crc32`, which is the same parameterization (init 0xFFFFFFFF,
        // reflected, final XOR 0xFFFFFFFF). Deriving them outside this crate is
        // the point: it pins the on-disk contract to a third party, so a pair of
        // implementations that agree with each other but are both wrong still
        // fails here rather than passing the differential test below.
        for (input, expected) in [
            (&b""[..], 0x0000_0000_u32),
            (&b"a"[..], 0xe8b7_be43),
            (&b"abc"[..], 0x3524_41c2),
            (&b"message digest"[..], 0x2015_9d7f),
            (&b"abcdefghijklmnopqrstuvwxyz"[..], 0x4c27_50bd),
            (&[0_u8; 32][..], 0x190a_55ad),
            (&[0xff_u8; 32][..], 0xff6c_ab0b),
        ] {
            assert_eq!(checksum(&[input]), expected, "accelerated {input:?}");
            assert_eq!(reference::checksum(&[input]), expected, "reference");
        }
    }

    /// Differential test: the accelerated implementation and the reference
    /// implementation must agree bit-for-bit.
    ///
    /// The corpus deliberately targets the places a fast CRC can diverge from a
    /// naive one:
    ///
    /// - every length from 0 to 300 bytes, so the SIMD entry/exit thresholds
    ///   and the scalar remainder loop are both crossed;
    /// - lengths straddling common 16/32/64-byte block boundaries;
    /// - degenerate byte patterns (all zero, all `0xff`) alongside
    ///   pseudo-random data;
    /// - the same logical byte stream split into differing numbers of parts,
    ///   which is how the WAL actually calls this (header slice + payload).
    #[test]
    fn accelerated_and_reference_agree_across_corpus() {
        let mut cases = 0_usize;

        // Contiguous inputs of every small length, three fill patterns each.
        for len in 0..=300_usize {
            for pattern in 0..3_u8 {
                let data = corpus_bytes(len, pattern);
                assert_eq!(
                    checksum(&[&data]),
                    reference::checksum(&[&data]),
                    "len={len} pattern={pattern}"
                );
                cases += 1;
            }
        }

        // Lengths straddling block boundaries a vectorized implementation is
        // likely to use internally.
        for len in [
            15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 257, 1023, 1024, 1025,
            4095, 4096, 4097, 8192, 65_535, 65_536,
        ] {
            let data = corpus_bytes(len, 3);
            assert_eq!(
                checksum(&[&data]),
                reference::checksum(&[&data]),
                "boundary len={len}"
            );
            cases += 1;
        }

        // Multi-part inputs: identical byte stream, different part splits. All
        // splits of one stream must also equal the contiguous checksum, which
        // is the property `format::encode_batch` relies on when it hashes the
        // header slice and payload separately.
        let stream = corpus_bytes(1000, 3);
        let contiguous = checksum(&[&stream]);
        for chunk in [1_usize, 2, 3, 7, 16, 64, 333, 999, 1000] {
            let parts: Vec<&[u8]> = stream.chunks(chunk).collect();
            assert_eq!(checksum(&parts), contiguous, "chunk={chunk}");
            assert_eq!(reference::checksum(&parts), contiguous, "chunk={chunk}");
            cases += 1;
        }

        // Empty parts interleaved must not perturb the result.
        assert_eq!(checksum(&[&[], &stream, &[]]), contiguous);
        assert_eq!(reference::checksum(&[&[], &stream, &[]]), contiguous);
        cases += 1;

        assert!(cases > 900, "corpus unexpectedly small: {cases}");
    }

    /// Deterministic corpus generator. `pattern` selects all-zero, all-`0xff`,
    /// a byte ramp, or an xorshift pseudo-random fill; no external RNG so the
    /// corpus is identical on every platform and run.
    fn corpus_bytes(len: usize, pattern: u8) -> Vec<u8> {
        match pattern {
            0 => vec![0_u8; len],
            1 => vec![0xff_u8; len],
            2 => (0..len)
                .map(|index| u8::try_from(index % 251).expect("modulo fits u8"))
                .collect(),
            _ => {
                let mut state = 0x2545_f491_4f6c_dd1d_u64 ^ (len as u64);
                (0..len)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        u8::try_from(state & 0xff).expect("masked value fits u8")
                    })
                    .collect()
            }
        }
    }
}
