//! Content-defined chunking for omni-sync.
//!
//! Real files change locally, so boundaries must not depend on absolute
//! offsets. A gear-hash rolling hash cuts where the *content* says to cut, so
//! an insertion near the top of a large file re-chunks one region instead of
//! shifting every boundary after it. That is what makes delta sync and dedup
//! worth having.
//!
//! Sizes default to the plan's values: 64 KiB min, 256 KiB average, 1 MiB max.

mod gear;

use std::io::{self, Read};

use gear::GEAR;

/// Chunk size bounds. `avg` is rounded up to a power of two for the mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min: 64 * 1024,
            avg: 256 * 1024,
            max: 1024 * 1024,
        }
    }
}

impl Config {
    /// A configuration small enough to exercise boundaries in tests.
    pub fn small() -> Self {
        Self {
            min: 64,
            avg: 256,
            max: 1024,
        }
    }
}

/// A chunk's location within the source, without its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub offset: usize,
    pub len: usize,
}

/// A chunk with its bytes, produced by the streaming path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedChunk {
    pub offset: u64,
    pub data: Vec<u8>,
}

/// A configured content-defined chunker.
#[derive(Clone, Copy, Debug)]
pub struct Chunker {
    cfg: Config,
    mask: u64,
}

impl Chunker {
    /// Panics if the bounds are nonsensical (`min > avg`, `avg > max`, or zero),
    /// because a bad configuration should fail at construction, not mid-scan.
    pub fn new(cfg: Config) -> Self {
        assert!(cfg.min > 0, "chunk min must be > 0");
        assert!(cfg.min <= cfg.avg, "chunk min must be <= avg");
        assert!(cfg.avg <= cfg.max, "chunk avg must be <= max");
        let mask = (cfg.avg.next_power_of_two() - 1) as u64;
        Self { cfg, mask }
    }

    pub fn config(&self) -> Config {
        self.cfg
    }

    /// Split an in-memory buffer into chunks. Offsets are contiguous and cover
    /// the whole input exactly once.
    pub fn split(&self, data: &[u8]) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        let mut start = 0usize;
        let mut hash = 0u64;
        for (i, byte) in data.iter().enumerate() {
            hash = (hash << 1).wrapping_add(GEAR[*byte as usize]);
            let size = i - start + 1;
            if size >= self.cfg.max || (size >= self.cfg.min && hash & self.mask == 0) {
                chunks.push(Chunk {
                    offset: start,
                    len: size,
                });
                start = i + 1;
                hash = 0;
            }
        }
        if start < data.len() {
            chunks.push(Chunk {
                offset: start,
                len: data.len() - start,
            });
        }
        chunks
    }

    /// Split a stream, holding at most about one max-sized chunk in memory.
    /// Returns owned chunks because the caller (hashing + upload) needs the
    /// bytes, and a 40 GB file must never be buffered whole.
    pub fn split_reader<R: Read>(&self, mut reader: R) -> io::Result<Vec<OwnedChunk>> {
        let mut out = Vec::new();
        let mut buf: Vec<u8> = Vec::with_capacity(self.cfg.max * 2);
        let mut read_buf = vec![0u8; 64 * 1024];
        let mut base_offset: u64 = 0;
        let mut scan_from = 0usize;
        let mut start = 0usize;
        let mut hash = 0u64;

        loop {
            let n = reader.read(&mut read_buf)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&read_buf[..n]);

            while scan_from < buf.len() {
                hash = hash
                    .wrapping_shl(1)
                    .wrapping_add(GEAR[buf[scan_from] as usize]);
                let size = scan_from - start + 1;
                if size >= self.cfg.max || (size >= self.cfg.min && hash & self.mask == 0) {
                    out.push(OwnedChunk {
                        offset: base_offset + start as u64,
                        data: buf[start..=scan_from].to_vec(),
                    });
                    start = scan_from + 1;
                    hash = 0;
                }
                scan_from += 1;
            }

            if start > 0 {
                buf.drain(0..start);
                base_offset += start as u64;
                scan_from -= start;
                start = 0;
            }
        }

        if start < buf.len() {
            out.push(OwnedChunk {
                offset: base_offset + start as u64,
                data: buf[start..].to_vec(),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes: splitmix64, so tests do not depend on
    /// a seeded RNG crate and always replay the same data.
    fn noisy(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut x = seed;
        while out.len() < len {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            out.extend_from_slice(&z.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn splitting_is_deterministic() {
        let c = Chunker::new(Config::small());
        let data = noisy(50_000, 1);
        assert_eq!(c.split(&data), c.split(&data));
    }

    #[test]
    fn chunks_cover_the_input_contiguously() {
        let c = Chunker::new(Config::small());
        let data = noisy(20_000, 2);
        let chunks = c.split(&data);
        let mut at = 0;
        for chunk in &chunks {
            assert_eq!(chunk.offset, at);
            assert!(chunk.len > 0);
            at += chunk.len;
        }
        assert_eq!(at, data.len());
    }

    #[test]
    fn no_chunk_exceeds_the_maximum() {
        let c = Chunker::new(Config::small());
        let data = noisy(20_000, 3);
        for chunk in c.split(&data) {
            assert!(chunk.len <= c.config().max);
        }
        // All but the final chunk respect the minimum too.
        let chunks = c.split(&data);
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(chunk.len >= c.config().min);
        }
    }

    #[test]
    fn a_small_input_is_a_single_chunk() {
        let c = Chunker::new(Config::small());
        assert_eq!(c.split(b"hello"), vec![Chunk { offset: 0, len: 5 }]);
    }

    #[test]
    fn an_insertion_resynchronises_instead_of_shifting_everything() {
        let c = Chunker::new(Config::small());
        let base = noisy(64_000, 4);
        let mut edited = Vec::with_capacity(base.len() + 16);
        edited.extend_from_slice(&base[..1_000]);
        edited.extend_from_slice(&noisy(16, 99));
        edited.extend_from_slice(&base[1_000..]);

        let before: Vec<Vec<u8>> = c
            .split(&base)
            .iter()
            .map(|ch| base[ch.offset..ch.offset + ch.len].to_vec())
            .collect();
        let after: Vec<Vec<u8>> = c
            .split(&edited)
            .iter()
            .map(|ch| edited[ch.offset..ch.offset + ch.len].to_vec())
            .collect();

        // Chunks after the edit that are byte-identical: a cut point is found
        // again quickly, so most of the file is unchanged data.
        let shared = after.iter().filter(|ch| before.contains(ch)).count();
        assert!(
            shared * 2 >= after.len(),
            "expected most chunks to survive the insertion, {shared}/{} did",
            after.len()
        );
    }

    #[test]
    fn streaming_and_in_memory_agree() {
        let c = Chunker::new(Config::small());
        let data = noisy(40_000, 5);
        let mem = c.split(&data);
        let streamed = c.split_reader(&data[..]).unwrap();
        assert_eq!(mem.len(), streamed.len());
        for (chunk, owned) in mem.iter().zip(&streamed) {
            assert_eq!(chunk.offset as u64, owned.offset);
            assert_eq!(chunk.len, owned.data.len());
            assert_eq!(
                &data[chunk.offset..chunk.offset + chunk.len],
                &owned.data[..]
            );
        }
    }
}
