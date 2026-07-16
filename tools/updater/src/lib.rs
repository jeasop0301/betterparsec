//! Shared library for G006 complete-set packaging/updates: canonical
//! JSON, the signed manifest format, staged-set verification, and the
//! atomic install swap. Consumed by the `betterparsec-updater` binary
//! (`sign`, `swap`) and by `app-native/src/update.rs` (verify-only, no
//! signing, no swap — see that module for why).

pub mod canon;
pub mod manifest;
pub mod swap;
pub mod verify;

use ed25519_dalek::{SigningKey, VerifyingKey};

/// Dev-only Ed25519 signing key seed used by `betterparsec-updater sign`
/// to produce dev-signed manifests for local packaging and the test
/// suite. This exact seed — and therefore the keypair derived from it —
/// is throwaway dev/test material, never a production secret; it ships
/// in the repo only so a dev build can be independently packaged and
/// verified without any external key material.
///
/// Release procedure (concrete, not vague): a release build sets the
/// `BP_TRUST_ROOT_HEX` environment variable at *compile* time to the
/// release Ed25519 public key, 64 lowercase hex characters. That value
/// is picked up by `option_env!("BP_TRUST_ROOT_HEX")` in
/// `app-native/src/update.rs::trust_root`, parsed via
/// [`parse_trust_root_hex`], and used instead of [`dev_trust_root`] —
/// no production private key ever enters this repository, and an
/// unset/absent env var (the normal dev build) falls back to this dev
/// key untouched.
pub const DEV_SIGNING_KEY_SEED: [u8; 32] = [
    0x42, 0x45, 0x54, 0x54, 0x45, 0x52, 0x50, 0x41, 0x52, 0x53, 0x45, 0x43, 0x2d, 0x64, 0x65, 0x76,
    0x2d, 0x6b, 0x65, 0x79, 0x2d, 0x30, 0x30, 0x31, 0x2d, 0x6e, 0x6f, 0x74, 0x2d, 0x66, 0x6f, 0x72,
];

pub fn dev_signing_key() -> SigningKey {
    SigningKey::from_bytes(&DEV_SIGNING_KEY_SEED)
}

pub fn dev_trust_root() -> VerifyingKey {
    dev_signing_key().verifying_key()
}

/// Parse a 64-lowercase-hex-character Ed25519 public key — the shape
/// `BP_TRUST_ROOT_HEX` is expected to hold at build time (see
/// [`DEV_SIGNING_KEY_SEED`]'s doc for the release procedure this feeds,
/// and `app-native/src/update.rs::trust_root` for the call site). Kept
/// here (rather than duplicated in `app-native`) so the app never needs
/// its own `hex` dependency just to parse a build-time trust root.
pub fn parse_trust_root_hex(hex_str: &str) -> Result<VerifyingKey, String> {
    let bytes =
        hex::decode(hex_str).map_err(|e| format!("BP_TRUST_ROOT_HEX is not valid hex: {e}"))?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "BP_TRUST_ROOT_HEX must decode to exactly 32 bytes".to_string())?;
    VerifyingKey::from_bytes(&array)
        .map_err(|e| format!("BP_TRUST_ROOT_HEX is not a valid Ed25519 public key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the actual dev public key hex rather than merely asserting
    /// self-consistency, so a change to `DEV_SIGNING_KEY_SEED` (or a
    /// dependency upgrade that changes key derivation) is caught here
    /// instead of silently drifting the whole dev signing/verification
    /// chain.
    #[test]
    fn dev_trust_root_hex_is_pinned() {
        assert_eq!(
            hex::encode(dev_trust_root().to_bytes()),
            "fe7be644afd770b8446e954d34914ddf05934582f7454bb863c66862ea607ac0"
        );
    }

    #[test]
    fn parse_trust_root_hex_round_trips_the_dev_key() {
        let hex_str = hex::encode(dev_trust_root().to_bytes());
        assert_eq!(parse_trust_root_hex(&hex_str).expect("valid hex"), dev_trust_root());
    }

    #[test]
    fn parse_trust_root_hex_rejects_malformed_hex() {
        assert!(parse_trust_root_hex("not hex").is_err());
    }

    #[test]
    fn parse_trust_root_hex_rejects_wrong_length() {
        assert!(parse_trust_root_hex("abcd").is_err());
    }
}
