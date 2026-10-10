//! Tokens, pairing codes and the dashboard password.
//!
//! Device tokens are random and shown once; only their SHA-256 is stored. The
//! dashboard password is Argon2id (PHC string). Neither secret ever lands in the
//! config file in the clear.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::Rng as _;
use sha2::{Digest, Sha256};

/// Generate a new device bearer token (`osn_` + 32 random bytes, hex).
pub fn new_device_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    format!("osn_{}", hex::encode(bytes))
}

/// SHA-256 of a token, as stored in `devices.token_hash`.
pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// A human-typeable one-time pairing code, e.g. `ABCDE-FGHJK`. The alphabet
/// omits `I`, `O`, `0` and `1` so the code can be read aloud without ambiguity.
pub fn new_pairing_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    let mut code = String::with_capacity(11);
    for i in 0..11 {
        if i == 5 {
            code.push('-');
        } else {
            let index = rng.random_range(0..ALPHABET.len());
            code.push(ALPHABET[index] as char);
        }
    }
    code
}

/// Hash a password into an Argon2id PHC string.
pub fn hash_password(password: &str) -> Result<String, String> {
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| e.to_string())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| e.to_string())
}

/// Verify a password against a stored PHC string. Any parse failure is `false`,
/// so a corrupt hash fails closed.
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A random dashboard password for `init` when none was supplied.
pub fn random_password() -> String {
    let mut bytes = [0u8; 12];
    rand::rng().fill(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_codes_have_the_expected_shape() {
        let token = new_device_token();
        assert!(token.starts_with("osn_"));
        assert_eq!(token.len(), 4 + 64);
        assert_ne!(token, new_device_token(), "tokens must not repeat");

        let code = new_pairing_code();
        assert_eq!(code.len(), 11);
        assert_eq!(&code[5..6], "-");
    }

    #[test]
    fn password_verification_is_exact() {
        let phc = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong", &phc));
        assert!(!verify_password("wrong", "not-a-phc-string"));
    }

    #[test]
    fn token_hashing_is_stable() {
        assert_eq!(hash_token("osn_abc"), hash_token("osn_abc"));
        assert_ne!(hash_token("osn_abc"), hash_token("osn_abd"));
    }
}
