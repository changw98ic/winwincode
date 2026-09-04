// SPDX-License-Identifier: Apache-2.0

//! Argon2id password hashing for the multi-user login surface.
//!
//! Plaintext passwords exist only as call arguments. The stored form is the
//! opaque PHC string produced by Argon2id, which is also the only form the
//! `UserAccount` domain model accepts as `passwordHash`.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

const SALT_BYTES: usize = 16;

/// Hash one plaintext password into its Argon2id PHC form.
///
/// # Errors
///
/// Reports entropy or encoding failures without exposing the password.
pub fn hash_password(password: &str) -> Result<String, ()> {
    let mut salt_material = [0_u8; SALT_BYTES];
    getrandom::fill(&mut salt_material).map_err(|_| ())?;
    let salt = SaltString::encode_b64(&salt_material).map_err(|_| ())?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| ())?;
    Ok(hash.to_string())
}

/// Verify one plaintext password against its stored Argon2id PHC form.
#[must_use]
pub fn verify_password(password: &str, password_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(password_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_into_canonical_argon2id_phc_and_verifies() {
        let hash = hash_password("correct horse battery staple").expect("hash password");
        assert!(hash.starts_with("$argon2id$v=19$"));
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn produces_distinct_hashes_for_equal_passwords() {
        let first = hash_password("same-password").expect("first hash");
        let second = hash_password("same-password").expect("second hash");
        assert_ne!(first, second);
        assert!(verify_password("same-password", &first));
        assert!(verify_password("same-password", &second));
    }

    #[test]
    fn verification_fails_closed_on_corrupt_stored_hashes() {
        assert!(!verify_password("password", "not-a-phc"));
        assert!(!verify_password(
            "password",
            "$argon2id$v=19$m=19456,t=2,p=1$",
        ));
    }
}
