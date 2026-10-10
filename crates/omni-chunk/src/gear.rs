//! The gear table: 256 fixed, deterministic 64-bit constants, one per byte
//! value. Generated at compile time with splitmix64 so the table is stable
//! across runs and platforms — a chunk boundary that moved because the table
//! changed would silently re-upload the world.
//!
//! The table pairs with a *shifting* rolling hash (`h = (h << 1) + GEAR[b]`),
//! not a rotating one. Shifting is what makes the hash depend on a bounded
//! window of recent bytes: contributions older than 64 positions fall off the
//! top. A rotate would keep every byte's contribution forever, so an insertion
//! near the start would shift every later boundary and nothing would ever
//! resynchronise.

pub const GEAR: [u64; 256] = build();

const fn build() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let mut i = 0;
    while i < 256 {
        state = splitmix(state);
        table[i] = state;
        i += 1;
    }
    table
}

const fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
