//! Capture the exact Rust target triple the release workflow built for.
//!
//! `TARGET` is only available to build scripts, so it is re-emitted as
//! `OMNI_TARGET` for `env!` at compile time. That makes asset selection
//! match the release naming exactly, with no runtime OS/arch table to drift
//! from `.github/workflows/release.yml`.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=OMNI_TARGET={target}");
}
