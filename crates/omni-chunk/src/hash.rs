//! Content hashing: chunk addresses are SHA-256 of the chunk's bytes.

use sha2::{Digest, Sha256};

/// SHA-256 of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Lowercase-hex SHA-256 of `bytes` — the form chunk addresses use.
pub fn hash_hex(bytes: &[u8]) -> String {
    hex::encode(hash_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_known_sha256_of_abc() {
        assert_eq!(
            hash_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn empty_input_has_the_known_digest() {
        assert_eq!(
            hash_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
