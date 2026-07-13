use openssl::{hash::MessageDigest, pkcs5, rand::rand_bytes};

use crate::app::AppError;

pub const HASH_ITERATIONS: u32 = 600_000;

#[derive(Clone)]
pub struct StoragePassword {
    pub salt: [u8; 16],
    pub hash: [u8; 32],
    pub iterations: u32,
}

impl StoragePassword {
    fn hash(
        salt: &[u8; 16],
        iterations: u32,
        password: &str,
        out: &mut [u8; 32],
    ) -> Result<(), AppError> {
        if password.is_empty() {
            return Err(AppError::PasswordEmpty);
        }

        pkcs5::pbkdf2_hmac(
            password.as_bytes(),
            salt,
            iterations as usize,
            MessageDigest::sha256(),
            out,
        )?;

        Ok(())
    }

    pub fn new(password: &str) -> Result<Self, AppError> {
        let mut salt = [0u8; 16];

        rand_bytes(&mut salt)?;

        let mut hash = [0u8; 32];

        Self::hash(&salt, HASH_ITERATIONS, password, &mut hash)?;

        Ok(Self {
            salt,
            hash,
            iterations: HASH_ITERATIONS,
        })
    }

    pub fn verify(&self, password: &str) -> Result<bool, AppError> {
        let mut hash = [0u8; 32];
        Self::hash(&self.salt, self.iterations, password, &mut hash)?;

        // Constant-time comparison (OpenSSL CRYPTO_memcmp): a plain `==` on the
        // PBKDF2 hash short-circuits on the first differing byte, leaking a timing
        // oracle across many login attempts. Both slices are always 32 bytes, so
        // `openssl::memcmp::eq` (which requires equal lengths) never panics.
        Ok(openssl::memcmp::eq(&self.hash, &hash))
    }

    pub fn needs_rehash(&self) -> bool {
        self.iterations < HASH_ITERATIONS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_accepts_correct_and_rejects_wrong() {
        let p = StoragePassword::new("correct horse battery staple")
            .expect("test password creation must succeed");
        assert!(
            p.verify("correct horse battery staple")
                .expect("correct password verification must succeed")
        );
        assert!(
            !p.verify("wrong password")
                .expect("wrong password verification must complete")
        );
        // A one-char difference must still reject (constant-time eq is still eq).
        assert!(
            !p.verify("correct horse battery stapl")
                .expect("near-match password verification must complete")
        );
    }

    #[test]
    fn verify_empty_password_errs() {
        let p = StoragePassword::new("nonempty")
            .expect("non-empty test password creation must succeed");
        assert!(matches!(p.verify(""), Err(AppError::PasswordEmpty)));
    }

    #[test]
    fn new_rejects_empty_password() {
        assert!(matches!(
            StoragePassword::new(""),
            Err(AppError::PasswordEmpty)
        ));
    }

    #[test]
    fn distinct_salts_make_distinct_hashes() {
        // Same password, two instances → different random salts → different stored
        // hashes, yet both verify. Guards against a regression to unsalted hashing.
        let a = StoragePassword::new("same").expect("first test password must be created");
        let b = StoragePassword::new("same").expect("second test password must be created");
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.hash, b.hash);
        assert!(
            a.verify("same")
                .expect("first test password verification must complete")
        );
        assert!(
            b.verify("same")
                .expect("second test password verification must complete")
        );
    }
}
