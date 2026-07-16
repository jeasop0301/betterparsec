//! coturn `use-auth-secret` ephemeral TURN credentials (betterparsec feature #2).
//!
//! Implements the TURN REST API scheme coturn accepts with `use-auth-secret`:
//! - `username   = "{expiry_unix}:{name}"` (or just `"{expiry_unix}"` if name empty)
//! - `credential = base64( HMAC-SHA1( static_auth_secret, username ) )`
//!
//! Short-lived credentials mean the browser gets a fresh, time-boxed TURN login
//! each session instead of a static secret baked into the client — see
//! docs/ARCHITECTURE.md feature #2. This is pure/deterministic so it is
//! unit-tested (incl. an RFC 2202 known-answer vector) without any live TURN
//! server; the end-to-end relay connect still needs a deployed coturn (M1).

use openssl::{base64, error::ErrorStack, hash::MessageDigest, pkey::PKey, sign::Signer};

/// A time-boxed TURN login for one session.
#[derive(Debug, Clone)]
pub struct TurnCredentials {
    /// `"{expiry_unix}:{name}"` — coturn checks the embedded expiry.
    pub username: String,
    /// base64(HMAC-SHA1(secret, username)).
    pub credential: String,
}

fn hmac_sha1(secret: &[u8], message: &[u8]) -> Result<Vec<u8>, ErrorStack> {
    let key = PKey::hmac(secret)?;
    let mut signer = Signer::new(MessageDigest::sha1(), &key)?;
    signer.update(message)?;
    signer.sign_to_vec()
}

/// Generate credentials valid until `now_unix + ttl_secs`.
///
/// `now_unix` is injected (not read from the clock) so the function stays pure
/// and testable. `ttl_secs` is saturating so no realistic input can overflow.
pub fn generate(
    secret: &str,
    name: &str,
    ttl_secs: u64,
    now_unix: u64,
) -> Result<TurnCredentials, ErrorStack> {
    let expiry = now_unix.saturating_add(ttl_secs);
    let username = if name.is_empty() {
        expiry.to_string()
    } else {
        format!("{expiry}:{name}")
    };
    let credential = base64::encode_block(&hmac_sha1(secret.as_bytes(), username.as_bytes())?);
    Ok(TurnCredentials {
        username,
        credential,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 2202 HMAC-SHA1 test case 2 — a trusted known-answer vector proving we
    /// actually compute HMAC-SHA1 (not some other/truncated MAC).
    #[test]
    fn hmac_sha1_matches_rfc2202() {
        let mac = hmac_sha1(b"Jefe", b"what do ya want for nothing?").expect("hmac");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
    }

    #[test]
    fn username_encodes_expiry_and_name() {
        let c = generate("s", "alice", 600, 1_700_000_000).expect("generate");
        assert_eq!(c.username, "1700000600:alice");
    }

    #[test]
    fn empty_name_omits_separator() {
        let c = generate("s", "", 600, 1_700_000_000).expect("generate");
        assert_eq!(c.username, "1700000600");
    }

    #[test]
    fn credential_is_base64_of_hmac_over_username() {
        let c = generate("secret", "user", 3600, 1_000_000).expect("generate");
        let decoded = base64::decode_block(&c.credential).expect("base64");
        assert_eq!(decoded.len(), 20, "SHA1 digest is 20 bytes");
        assert_eq!(
            decoded,
            hmac_sha1(b"secret", c.username.as_bytes()).expect("hmac")
        );
    }

    #[test]
    fn deterministic_for_same_inputs() {
        let a = generate("s", "u", 60, 1000).expect("generate");
        let b = generate("s", "u", 60, 1000).expect("generate");
        assert_eq!(a.credential, b.credential);
        assert_eq!(a.username, b.username);
    }

    #[test]
    fn sensitive_to_secret_name_and_expiry() {
        let base = generate("s", "u", 60, 1000).expect("generate").credential;
        assert_ne!(
            base,
            generate("s2", "u", 60, 1000).expect("generate").credential
        );
        assert_ne!(
            base,
            generate("s", "u2", 60, 1000).expect("generate").credential
        );
        // expiry is embedded in the signed username, so a different time changes it.
        assert_ne!(
            base,
            generate("s", "u", 60, 1001).expect("generate").credential
        );
    }

    #[test]
    fn ttl_saturates_without_panic() {
        let c = generate("s", "u", u64::MAX, u64::MAX).expect("generate");
        assert!(c.username.starts_with(&u64::MAX.to_string()));
    }
}
