//! Regenerate the chunker golden vectors committed at
//! `crates/omni-chunk/testdata/chunk_vectors.json`.
//!
//! ```sh
//! cargo run -p omni-chunk --example gen_vectors > crates/omni-chunk/testdata/chunk_vectors.json
//! ```
//!
//! The Obsidian plugin vendors a copy of the JSON and asserts the same offsets
//! with its own implementation.

use omni_chunk::testdata;
use omni_chunk::{Chunker, Config};

fn main() {
    let config = Config::default();
    let chunker = Chunker::new(config);
    let cases: &[(&str, usize, u64)] = &[
        ("splitmix64", 2 * 1024 * 1024, 1),
        ("splitmix64", 300_000, 7),
        ("ascii-ramp", 1024 * 1024, 0),
        ("text", 1024 * 1024, 0),
        ("ascii-noise", 1024 * 1024, 3),
    ];

    println!("{{");
    println!(
        "  \"config\": {{ \"min\": {}, \"avg\": {}, \"max\": {} }},",
        config.min, config.avg, config.max
    );
    println!("  \"cases\": [");
    for (index, (generator, len, seed)) in cases.iter().enumerate() {
        let data = testdata::fixture(generator, *len, *seed);
        let lens: Vec<String> = chunker
            .split(&data)
            .iter()
            .map(|chunk| chunk.len.to_string())
            .collect();
        let comma = if index + 1 == cases.len() { "" } else { "," };
        println!(
            "    {{ \"name\": \"{generator}-{len}\", \"generator\": \"{generator}\", \"len\": {len}, \"seed\": {seed}, \"lens\": [{}] }}{comma}",
            lens.join(", ")
        );
    }
    println!("  ]");
    println!("}}");
}
