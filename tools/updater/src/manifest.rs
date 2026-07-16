//! G006 complete-set manifest: the signed inventory of every file that
//! makes up one deployable client+host set, plus the protocol versions
//! that set speaks. Signing happens only in the `sign` CLI subcommand
//! (dev key, packaging-time); verification (`crate::verify`) is shared
//! by `swap` and the in-app verify-only module
//! (`app-native/src/update.rs`) — `crate::swap::perform_swap` requires
//! a `crate::verify::VerifiedSet` token that only
//! `crate::verify::verify_staged_set` can produce, so nothing in this
//! crate can install a set it never actually verified.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::canon::{CanonError, canonicalize_serialize};

/// Filename the signed manifest is always written under inside a staged
/// directory (see `app-native/src/update.rs`'s module doc for the full
/// staged-set layout this pins).
pub const MANIFEST_FILE_NAME: &str = "manifest.json";

/// FEC wire protocol this set was built against (see
/// `client-transport/src/flow.rs`, `streamer/src/main.rs`:
/// `select_fec_protocol`). `2` is FEC v2 (epoch-based sender renegotiation).
pub const FEC_PROTOCOL_VERSION: u32 = 2;

/// Version of the `ConnectionTerminated { error_code }` code table
/// documented on `common::api_bindings::StreamServerMessage::ConnectionTerminated`
/// (moonlight-common band `-100..=-104` / `0`, plus the G003 FEC-sender
/// fail-closed band `-1..=-4`). Bump this whenever that code table gains
/// or repurposes a code, so an old client/host pairing never
/// misinterprets a new code as one of its own meanings.
pub const CONNECTION_TERMINATED_CODE_TABLE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Client,
    Host,
    Shared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentEntry {
    /// Relative path under the staged/install root, e.g. `betterparsec.exe`.
    pub name: String,
    pub role: Role,
    /// Lowercase hex SHA-256 of the file's bytes.
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersions {
    pub fec: u32,
    pub connection_terminated_codes: u32,
}

impl Default for ProtocolVersions {
    fn default() -> Self {
        Self {
            fec: FEC_PROTOCOL_VERSION,
            connection_terminated_codes: CONNECTION_TERMINATED_CODE_TABLE_VERSION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub set_version: String,
    pub protocol: ProtocolVersions,
    pub components: Vec<ComponentEntry>,
    /// Identifies which trust-root public key this manifest is expected
    /// to verify against (dev vs. a future release key id). Purely
    /// informational for the verifier — the actual key bytes always come
    /// from the compiled-in trust root, never from the manifest itself.
    pub signing_key_id: String,
}

impl Manifest {
    /// Canonical (RFC 8785 JCS subset) bytes this manifest is signed
    /// over. Deterministic regardless of `components` insertion order
    /// because canonicalization sorts object keys, but *not* regardless
    /// of `components` array order — callers should keep a stable
    /// ordering (e.g. sorted by `name`) when building a manifest.
    /// Fails when the manifest somehow contains a non-integer JSON
    /// number (it never should — every numeric field here is a `u32`/
    /// `u64` — but canonicalization enforces that loudly rather than
    /// silently emitting non-canonical bytes; see `crate::canon`).
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, VerifyError> {
        Ok(canonicalize_serialize(self)?.into_bytes())
    }

    /// `name -> sha256` for every component, used by compatibility checks
    /// that need to compare hashes across roles for the same file name.
    pub fn hashes_by_name(&self) -> BTreeMap<&str, &str> {
        self.components
            .iter()
            .map(|c| (c.name.as_str(), c.sha256.as_str()))
            .collect()
    }

    /// A manifest is internally consistent when every component sharing
    /// a `name` (e.g. `betterparsec.exe` referenced by both the client
    /// and host roles) records the *same* hash and size. A mismatch here
    /// means the set was assembled from mixed builds and must never be
    /// installed.
    pub fn check_internally_consistent(&self) -> Result<(), VerifyError> {
        let mut seen: BTreeMap<&str, (&str, u64)> = BTreeMap::new();
        for c in &self.components {
            match seen.get(c.name.as_str()) {
                Some((sha256, size)) if *sha256 != c.sha256 || *size != c.size => {
                    return Err(VerifyError::MixedVersionSet {
                        name: c.name.clone(),
                    });
                }
                _ => {
                    seen.insert(&c.name, (&c.sha256, c.size));
                }
            }
        }
        Ok(())
    }

    /// Compatibility with the running build: protocol versions must
    /// match exactly (no forward/backward negotiation at the update
    /// layer — a mismatched set is simply not installable by this build).
    pub fn check_protocol_compatible(&self, running: &ProtocolVersions) -> Result<(), VerifyError> {
        if self.protocol.fec != running.fec
            || self.protocol.connection_terminated_codes != running.connection_terminated_codes
        {
            return Err(VerifyError::ProtocolMismatch {
                manifest: self.protocol,
                running: *running,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedManifest {
    pub manifest: Manifest,
    /// Lowercase hex Ed25519 signature over `manifest.canonical_bytes()`.
    pub signature: String,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("signature does not match the trust root")]
    BadSignature,
    #[error("signature hex is malformed: {0}")]
    MalformedSignature(String),
    #[error("component {name:?} appears with conflicting hash/size (mixed-version set)")]
    MixedVersionSet { name: String },
    #[error("manifest protocol versions {manifest:?} do not match this build's {running:?}")]
    ProtocolMismatch {
        manifest: ProtocolVersions,
        running: ProtocolVersions,
    },
    #[error("component {name:?} is missing from the staged directory: {reason}")]
    MissingComponent { name: String, reason: String },
    #[error("component {name:?} size mismatch: manifest={expected} actual={actual}")]
    SizeMismatch {
        name: String,
        expected: u64,
        actual: u64,
    },
    #[error("component {name:?} sha256 mismatch")]
    HashMismatch { name: String },
    #[error("failed to canonicalize the manifest for signing/verification: {0}")]
    Canonicalization(#[from] CanonError),
    #[error("file {name:?} is present in the staged set but not listed in the manifest")]
    UnlistedFile { name: String },
    #[error("failed to enumerate the staged directory: {0}")]
    EnumerationFailed(String),
    #[error("component name {name:?} is not a safe relative path (must not be absolute or contain '..')")]
    UnsafeComponentName { name: String },
}

/// Reject component names that could escape the staged/install root:
/// absolute paths and any `..` path segment (checked on both `/` and
/// `\` separators so the check behaves the same on every target).
/// Called both when building a manifest (`sign` CLI, so a malicious or
/// buggy component spec never gets hashed and signed in the first
/// place) and when verifying one (`verify_component_file`, so an
/// attacker controlling only the manifest can't smuggle a traversal
/// component name into signature-covered bytes and win an install-time
/// escape).
pub fn validate_component_name(name: &str) -> Result<(), VerifyError> {
    let is_absolute = name.starts_with('/')
        || name.starts_with('\\')
        || name.get(1..2) == Some(":");
    let has_parent_segment = name.split(['/', '\\']).any(|segment| segment == "..");
    if is_absolute || has_parent_segment {
        return Err(VerifyError::UnsafeComponentName {
            name: name.to_string(),
        });
    }
    Ok(())
}

/// Sign `manifest` with a dev/release Ed25519 signing key. Only ever
/// called from the `sign` CLI subcommand — never from the in-app
/// verify-only module.
pub fn sign(manifest: Manifest, signing_key: &SigningKey) -> Result<SignedManifest, VerifyError> {
    let bytes = manifest.canonical_bytes()?;
    let signature: Signature = signing_key.sign(&bytes);
    Ok(SignedManifest {
        manifest,
        signature: hex::encode(signature.to_bytes()),
    })
}

/// Verify a [`SignedManifest`]'s signature against `trust_root`. This is
/// the *only* signature check in the whole update path — `swap` and the
/// in-app module both call this instead of re-implementing verification.
pub fn verify_signature(
    signed: &SignedManifest,
    trust_root: &VerifyingKey,
) -> Result<(), VerifyError> {
    let sig_bytes = hex::decode(&signed.signature)
        .map_err(|e| VerifyError::MalformedSignature(e.to_string()))?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| VerifyError::MalformedSignature("signature is not 64 bytes".into()))?;
    let signature = Signature::from_bytes(&sig_array);
    let bytes = signed.manifest.canonical_bytes()?;
    trust_root
        .verify(&bytes, &signature)
        .map_err(|_| VerifyError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev_key() -> SigningKey {
        // Deterministic 32-byte seed — a real signing key, but only ever
        // used in tests here; the actual dev trust root lives in
        // `crate::dev_trust_root` / `app-native/src/update.rs`.
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn sample_manifest() -> Manifest {
        Manifest {
            set_version: "2026.7.1".into(),
            protocol: ProtocolVersions::default(),
            components: vec![
                ComponentEntry {
                    name: "betterparsec.exe".into(),
                    role: Role::Client,
                    sha256: "a".repeat(64),
                    size: 100,
                },
                ComponentEntry {
                    name: "streamer.exe".into(),
                    role: Role::Host,
                    sha256: "b".repeat(64),
                    size: 200,
                },
            ],
            signing_key_id: "dev-trust-root-v1".into(),
        }
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let key = dev_key();
        let signed = sign(sample_manifest(), &key).expect("sign");
        assert!(verify_signature(&signed, &key.verifying_key()).is_ok());
    }

    #[test]
    fn tampering_a_component_hash_breaks_the_signature() {
        let key = dev_key();
        let mut signed = sign(sample_manifest(), &key).expect("sign");
        signed.manifest.components[0].sha256 = "c".repeat(64);
        assert_eq!(
            verify_signature(&signed, &key.verifying_key()),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn tampering_the_manifest_metadata_breaks_the_signature() {
        let key = dev_key();
        let mut signed = sign(sample_manifest(), &key).expect("sign");
        signed.manifest.set_version = "9999.9.9".into();
        assert_eq!(
            verify_signature(&signed, &key.verifying_key()),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn wrong_trust_root_is_rejected() {
        let key = dev_key();
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let signed = sign(sample_manifest(), &key).expect("sign");
        assert_eq!(
            verify_signature(&signed, &other.verifying_key()),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn internally_consistent_manifest_passes() {
        assert!(sample_manifest().check_internally_consistent().is_ok());
    }

    #[test]
    fn mixed_version_set_is_rejected() {
        let mut manifest = sample_manifest();
        // Same name, different hash/size: a set stitched together from
        // two incompatible builds.
        manifest.components.push(ComponentEntry {
            name: "betterparsec.exe".into(),
            role: Role::Host,
            sha256: "d".repeat(64),
            size: 999,
        });
        assert_eq!(
            manifest.check_internally_consistent(),
            Err(VerifyError::MixedVersionSet {
                name: "betterparsec.exe".into()
            })
        );
    }

    #[test]
    fn protocol_mismatch_is_rejected() {
        let manifest = sample_manifest();
        let running = ProtocolVersions {
            fec: 1,
            ..ProtocolVersions::default()
        };
        assert!(manifest.check_protocol_compatible(&running).is_err());
    }

    #[test]
    fn protocol_match_passes() {
        let manifest = sample_manifest();
        assert!(
            manifest
                .check_protocol_compatible(&ProtocolVersions::default())
                .is_ok()
        );
    }

    #[test]
    fn canonical_bytes_are_stable_json_with_sorted_keys() {
        let manifest = sample_manifest();
        let bytes = manifest.canonical_bytes().expect("integers only");
        let text = String::from_utf8(bytes).expect("canonical bytes are UTF-8");
        assert!(text.starts_with(r#"{"components":["#));
        assert!(!text.contains(' '));
    }

    #[test]
    fn canonical_bytes_rejects_a_manifest_that_somehow_serializes_a_float() {
        // Every numeric field on `Manifest`/`ComponentEntry` is a u32/u64,
        // so this can't happen through the normal API -- this test pins
        // the *propagation* path (canon::CanonError -> VerifyError)
        // rather than manufacturing an impossible `Manifest` value.
        let value = serde_json::json!({"not": "a manifest", "size": 1.5});
        let err = crate::canon::canonicalize_serialize(&value).expect_err("float rejected");
        let verify_err: VerifyError = err.into();
        assert!(matches!(verify_err, VerifyError::Canonicalization(_)));
    }

    #[test]
    fn validate_component_name_accepts_plain_relative_names() {
        assert!(validate_component_name("betterparsec.exe").is_ok());
        assert!(validate_component_name("assets/betterparsec-portable.zip").is_ok());
    }

    #[test]
    fn validate_component_name_rejects_parent_dir_traversal() {
        assert!(validate_component_name("..\\evil").is_err());
        assert!(validate_component_name("../evil").is_err());
        assert!(validate_component_name("nested/../evil").is_err());
    }

    #[test]
    fn validate_component_name_rejects_absolute_paths() {
        assert!(validate_component_name("/etc/passwd").is_err());
        assert!(validate_component_name("\\\\server\\share").is_err());
        assert!(validate_component_name("C:\\evil").is_err());
    }
}
