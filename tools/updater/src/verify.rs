//! Per-file verification of a staged complete-set directory against a
//! signed manifest. Every check here (signature, internal consistency,
//! protocol compatibility, per-file hash/size, no unlisted extra files)
//! is re-run independently by both `swap` and the in-app verify-only
//! module — nobody gets to skip a step by asserting "it's already
//! verified". [`verify_staged_set`] is the *only* function in this
//! crate that can produce a [`VerifiedSet`], and [`crate::swap::perform_swap`]
//! requires one — so "swap only ever installs something this crate
//! itself verified" is enforced by the type system, not just by this
//! doc comment.

use std::fs;
use std::io::Read;
use std::path::Path;

use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};

use crate::manifest::{self, MANIFEST_FILE_NAME, ProtocolVersions, SignedManifest, VerifyError};

/// Proof that [`verify_staged_set`] already verified a particular staged
/// directory against its signed manifest. Opaque and only constructible
/// by this module (a private field gates construction), so a caller
/// can't manufacture one to skip verification — it has to actually call
/// [`verify_staged_set`] and get `Ok` back.
pub struct VerifiedSet(());

impl VerifiedSet {
    /// Construct one directly for tests exercising `perform_swap` in
    /// isolation from a full signed manifest + staged directory + trust
    /// root. Not reachable outside this crate, and only compiled into
    /// test builds.
    #[cfg(test)]
    pub(crate) fn assume_verified_for_test() -> Self {
        Self(())
    }
}

/// Full verification of a staged directory against a signed manifest:
/// signature, internal consistency (no mixed-version components),
/// protocol compatibility with this build, every component's SHA-256/
/// size, and that the staged directory contains no file the manifest
/// doesn't list (the only file allowed to be present-but-unlisted is
/// the manifest itself, `manifest.json`, which necessarily can't list
/// itself as a component). Returns a [`VerifiedSet`] token only when
/// every check passes.
pub fn verify_staged_set(
    staged_dir: &Path,
    signed: &SignedManifest,
    trust_root: &VerifyingKey,
    running_protocol: &ProtocolVersions,
) -> Result<VerifiedSet, VerifyError> {
    manifest::verify_signature(signed, trust_root)?;
    signed.manifest.check_internally_consistent()?;
    signed
        .manifest
        .check_protocol_compatible(running_protocol)?;
    for component in &signed.manifest.components {
        manifest::validate_component_name(&component.name)?;
        verify_component_file(staged_dir, &component.name, &component.sha256, component.size)?;
    }
    check_no_unlisted_files(staged_dir, signed)?;
    Ok(VerifiedSet(()))
}

/// Recursively walk `staged_dir` and fail if any file present is not
/// exactly one of the manifest's listed components (allow-listing only
/// the manifest file itself, `manifest.json` at the staged root). This
/// closes the gap where a staged directory could carry an extra,
/// unsigned file that never gets hash-checked because nothing in the
/// manifest names it — such a file would otherwise ride along into the
/// install directory unexamined.
fn check_no_unlisted_files(staged_dir: &Path, signed: &SignedManifest) -> Result<(), VerifyError> {
    let listed: std::collections::BTreeSet<String> = signed
        .manifest
        .components
        .iter()
        .map(|c| c.name.replace('\\', "/"))
        .collect();

    let mut stack = vec![staged_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(VerifyError::EnumerationFailed(format!(
                    "reading directory {}: {e}",
                    dir.display()
                )));
            }
        };
        for entry in entries {
            let entry = entry.map_err(|e| {
                VerifyError::EnumerationFailed(format!(
                    "reading entry in {}: {e}",
                    dir.display()
                ))
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|e| {
                VerifyError::EnumerationFailed(format!(
                    "stat {}: {e}",
                    path.display()
                ))
            })?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(staged_dir)
                .expect("walked path is under staged_dir")
                .to_string_lossy()
                .replace('\\', "/");
            if rel == MANIFEST_FILE_NAME {
                continue;
            }
            if !listed.contains(rel.as_str()) {
                return Err(VerifyError::UnlistedFile { name: rel });
            }
        }
    }
    Ok(())
}

fn verify_component_file(
    staged_dir: &Path,
    name: &str,
    expected_sha256: &str,
    expected_size: u64,
) -> Result<(), VerifyError> {
    let path = staged_dir.join(name);
    let metadata = fs::metadata(&path).map_err(|e| VerifyError::MissingComponent {
        name: name.to_string(),
        reason: e.to_string(),
    })?;
    let actual_size = metadata.len();
    if actual_size != expected_size {
        return Err(VerifyError::SizeMismatch {
            name: name.to_string(),
            expected: expected_size,
            actual: actual_size,
        });
    }
    let actual_hash = sha256_file(&path).map_err(|e| VerifyError::MissingComponent {
        name: name.to_string(),
        reason: e.to_string(),
    })?;
    if !actual_hash.eq_ignore_ascii_case(expected_sha256) {
        return Err(VerifyError::HashMismatch {
            name: name.to_string(),
        });
    }
    Ok(())
}

/// Lowercase hex SHA-256 of a file's bytes, streamed so packaging large
/// components (the Foundation payload) never loads a whole file into
/// memory at once.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ComponentEntry, Manifest, Role};
    use ed25519_dalek::SigningKey;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bp-updater-verify-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write_component(dir: &Path, name: &str, contents: &[u8]) -> ComponentEntry {
        fs::write(dir.join(name), contents).expect("write component");
        ComponentEntry {
            name: name.to_string(),
            role: Role::Client,
            sha256: sha256_file(&dir.join(name)).expect("hash"),
            size: contents.len() as u64,
        }
    }

    #[test]
    fn verifies_a_correct_staged_set() {
        let dir = temp_dir("ok");
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let component = write_component(&dir, "betterparsec.exe", b"client bytes");
        let manifest = Manifest {
            set_version: "1.0.0".into(),
            protocol: ProtocolVersions::default(),
            components: vec![component],
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = manifest::sign(manifest, &key).expect("sign");
        let result = verify_staged_set(
            &dir,
            &signed,
            &key.verifying_key(),
            &ProtocolVersions::default(),
        );
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_file_on_disk_fails_hash_check() {
        let dir = temp_dir("tamper-file");
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let component = write_component(&dir, "betterparsec.exe", b"client bytes");
        let manifest = Manifest {
            set_version: "1.0.0".into(),
            protocol: ProtocolVersions::default(),
            components: vec![component],
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = manifest::sign(manifest, &key).expect("sign");
        // Flip a byte on disk after signing — the manifest itself is
        // untouched (signature stays valid) but the staged file no
        // longer matches it.
        fs::write(dir.join("betterparsec.exe"), b"CLIENT BYTES").expect("tamper");
        let result = verify_staged_set(
            &dir,
            &signed,
            &key.verifying_key(),
            &ProtocolVersions::default(),
        );
        assert!(matches!(result, Err(VerifyError::HashMismatch { .. })));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_component_is_reported() {
        let dir = temp_dir("missing");
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let manifest = Manifest {
            set_version: "1.0.0".into(),
            protocol: ProtocolVersions::default(),
            components: vec![ComponentEntry {
                name: "streamer.exe".into(),
                role: Role::Host,
                sha256: "a".repeat(64),
                size: 10,
            }],
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = manifest::sign(manifest, &key).expect("sign");
        let result = verify_staged_set(
            &dir,
            &signed,
            &key.verifying_key(),
            &ProtocolVersions::default(),
        );
        assert!(matches!(result, Err(VerifyError::MissingComponent { .. })));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn idempotent_reverify_gives_the_same_result() {
        let dir = temp_dir("idempotent");
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let component = write_component(&dir, "betterparsec.exe", b"same bytes every time");
        let manifest = Manifest {
            set_version: "1.0.0".into(),
            protocol: ProtocolVersions::default(),
            components: vec![component],
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = manifest::sign(manifest, &key).expect("sign");
        for _ in 0..3 {
            assert!(
                verify_staged_set(
                    &dir,
                    &signed,
                    &key.verifying_key(),
                    &ProtocolVersions::default(),
                )
                .is_ok()
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_planted_extra_file_not_in_the_manifest_is_rejected() {
        let dir = temp_dir("extra-file");
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let component = write_component(&dir, "betterparsec.exe", b"client bytes");
        let manifest = Manifest {
            set_version: "1.0.0".into(),
            protocol: ProtocolVersions::default(),
            components: vec![component],
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = manifest::sign(manifest, &key).expect("sign");
        // Not listed anywhere in the manifest -- e.g. a payload smuggled
        // into the staged directory after signing.
        fs::write(dir.join("evil.dll"), b"unsigned payload").expect("plant extra file");
        let result = verify_staged_set(
            &dir,
            &signed,
            &key.verifying_key(),
            &ProtocolVersions::default(),
        );
        assert!(matches!(
            result,
            Err(VerifyError::UnlistedFile { name }) if name == "evil.dll"
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_extra_file_nested_in_a_subdirectory_is_also_rejected() {
        let dir = temp_dir("extra-nested");
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let component = write_component(&dir, "betterparsec.exe", b"client bytes");
        let manifest = Manifest {
            set_version: "1.0.0".into(),
            protocol: ProtocolVersions::default(),
            components: vec![component],
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = manifest::sign(manifest, &key).expect("sign");
        fs::create_dir_all(dir.join("assets")).expect("mkdir assets");
        fs::write(dir.join("assets").join("evil.zip"), b"unsigned").expect("plant nested extra");
        let result = verify_staged_set(
            &dir,
            &signed,
            &key.verifying_key(),
            &ProtocolVersions::default(),
        );
        assert!(matches!(result, Err(VerifyError::UnlistedFile { .. })));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_manifest_file_itself_is_allow_listed_and_does_not_fail_verification() {
        let dir = temp_dir("manifest-allowlisted");
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let component = write_component(&dir, "betterparsec.exe", b"client bytes");
        let manifest = Manifest {
            set_version: "1.0.0".into(),
            protocol: ProtocolVersions::default(),
            components: vec![component],
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = manifest::sign(manifest, &key).expect("sign");
        fs::write(
            dir.join(MANIFEST_FILE_NAME),
            serde_json::to_string(&signed).expect("serialize"),
        )
        .expect("write manifest inside staged dir");
        let result = verify_staged_set(
            &dir,
            &signed,
            &key.verifying_key(),
            &ProtocolVersions::default(),
        );
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn component_name_path_traversal_is_rejected() {
        let dir = temp_dir("traversal");
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let manifest = Manifest {
            set_version: "1.0.0".into(),
            protocol: ProtocolVersions::default(),
            components: vec![ComponentEntry {
                name: "..\\evil".into(),
                role: Role::Host,
                sha256: "a".repeat(64),
                size: 4,
            }],
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = manifest::sign(manifest, &key).expect("sign");
        let result = verify_staged_set(
            &dir,
            &signed,
            &key.verifying_key(),
            &ProtocolVersions::default(),
        );
        assert!(matches!(result, Err(VerifyError::UnsafeComponentName { .. })));
        let _ = fs::remove_dir_all(&dir);
    }
}
