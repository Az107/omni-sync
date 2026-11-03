//! Deterministic fixtures for the chunker's golden vectors.
//!
//! The Obsidian plugin (TypeScript) must cut bytes at exactly the same offsets
//! as this crate, or the two clients of one folder never deduplicate. These
//! generators are the shared input: the committed `testdata/chunk_vectors.json`
//! is produced from them here and replayed by the plugin's unit test.
//!
//! Not `#[cfg(test)]` because `examples/gen_vectors.rs` needs it too. It is
//! tiny, has no I/O and is marked `#[doc(hidden)]`.

/// One fixture's bytes.
///
/// `generator` is one of:
/// - `splitmix64`: the crate's deterministic pseudo-random stream (seed).
/// - `ascii-ramp`: `i % 256`.
/// - `text`: a repeated English line, truncated to `len`.
/// - `ascii-noise`: splitmix64 mapped into printable ASCII, so it is
///   non-periodic like real prose and actually produces boundaries.
pub fn fixture(generator: &str, len: usize, seed: u64) -> Vec<u8> {
    match generator {
        "splitmix64" => noisy(len, seed),
        "ascii-ramp" => (0..len).map(|index| (index % 256) as u8).collect(),
        "text" => {
            let line = b"# Heading\n\nThe quick brown fox jumps over the lazy dog.\n";
            let mut out = Vec::with_capacity(len);
            while out.len() < len {
                let take = (len - out.len()).min(line.len());
                out.extend_from_slice(&line[..take]);
            }
            out
        }
        "ascii-noise" => noisy(len, seed)
            .into_iter()
            .map(|byte| 32 + (byte % 95))
            .collect(),
        other => panic!("unknown fixture generator {other:?}"),
    }
}

/// The same splitmix64 stream the crate's tests use, byte for byte.
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
