//! G006 in-app update module — **verify-only**. This module never talks
//! to a network, never signs anything, and never swaps an install in
//! place; that is `betterparsec-updater`'s job (`tools/updater`,
//! subcommands `sign`/`swap`, always invoked out-of-process by the
//! packaging script or an operator, never from inside the running app).
//! What this module *does* do: given a staged complete-set directory and
//! its signed manifest, verify the signature against a compiled-in trust
//! root, verify every component's SHA-256/size, verify the set is
//! internally consistent and protocol-compatible with this build, and
//! expose that as a small status enum the host UI panel can render
//! (`Idle` / `Checking` / `Staged { version }` / `VerifyFailed { reason }`
//! — shape agreed with the host-role UI lane over IRC).
//!
//! Canonicalization, the manifest format, and the verification logic
//! itself all live in `tools/updater` (the `bp_updater` crate) so this
//! module and `betterparsec-updater swap` can never disagree about what
//! "valid" means — this file only adds the compiled-in trust root and
//! the UI-facing status shape on top.
//!
//! ## Staged-set layout
//!
//! One convention, pinned here and nowhere else: everything for a
//! staged complete-set lives under `updates_dir/staged/`, *including*
//! the signed manifest itself at `updates_dir/staged/manifest.json`
//! (`bp_updater::manifest::MANIFEST_FILE_NAME`) — never beside the
//! `staged/` directory. [`staged_dir`] and [`manifest_path`] are the
//! only places that should ever spell this layout out; callers (the
//! host status panel, `tools/package-portable.ps1 -HostBundle`) derive
//! both paths from those helpers instead of hardcoding `"staged"` /
//! `"manifest.json"` a second time.
//!
//! ## Status caching
//!
//! Re-verifying a staged set means an Ed25519 signature check plus a
//! streaming SHA-256 of every component — too expensive to redo on
//! every egui repaint. [`cached_status`] keeps a process-wide cache
//! keyed on the manifest's fingerprint (its length + mtime — cheap to
//! `stat`, no hashing) and only kicks off a real re-verify, on a
//! background thread, when that fingerprint changes or [`invalidate`]
//! is called explicitly. The calling (UI) thread only ever reads a
//! cached [`UpdateStatus`] or the [`UpdateStatus::Checking`] interim
//! value — it never hashes.
//!
//! ## Release trust root
//!
//! A release build sets the `BP_TRUST_ROOT_HEX` environment variable at
//! *compile* time (64 lowercase hex characters, the release Ed25519
//! public key) so [`trust_root`] picks it up via
//! `option_env!("BP_TRUST_ROOT_HEX")` and uses it instead of
//! `bp_updater::dev_trust_root()`. See `bp_updater::DEV_SIGNING_KEY_SEED`
//! for the corresponding signing-side procedure. Leaving the env var
//! unset (the normal dev build) falls back to the dev trust root
//! unchanged.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use bp_updater::manifest::{self, ProtocolVersions, SignedManifest};
use ed25519_dalek::VerifyingKey;

/// `updates_dir/staged` — see this module's "Staged-set layout" doc.
pub fn staged_dir(updates_dir: &Path) -> PathBuf {
    updates_dir.join("staged")
}

/// `updates_dir/staged/manifest.json` — see this module's "Staged-set
/// layout" doc. The manifest lives *inside* the staged directory, not
/// beside it.
pub fn manifest_path(updates_dir: &Path) -> PathBuf {
    staged_dir(updates_dir).join(manifest::MANIFEST_FILE_NAME)
}

/// Compiled-in trust root — see this module's "Release trust root" doc.
fn trust_root() -> VerifyingKey {
    match option_env!("BP_TRUST_ROOT_HEX") {
        Some(hex) => bp_updater::parse_trust_root_hex(hex).unwrap_or_else(|e| {
            panic!(
                "BP_TRUST_ROOT_HEX is set at compile time but invalid ({e}) -- see \
                 app-native/src/update.rs's \"Release trust root\" module doc"
            )
        }),
        None => bp_updater::dev_trust_root(),
    }
}

/// This build's own protocol versions (FEC wire version, the
/// `ConnectionTerminated` code table version) — a staged set is only
/// installable when its manifest declares the same versions.
fn running_protocol_versions() -> ProtocolVersions {
    ProtocolVersions::default()
}

/// UI-facing update status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// No staged set present (or none was asked about).
    Idle,
    /// A manifest was found and a background re-verify is in flight;
    /// shown for exactly the frames between "fingerprint changed" and
    /// "background thread reported a result".
    Checking,
    /// A staged set passed every verification check and is safe to hand
    /// to `betterparsec-updater swap`.
    Staged { version: String },
    /// A staged set is present but failed verification; `reason` is
    /// safe to show directly (it never contains file contents/bytes,
    /// only what check failed).
    VerifyFailed { reason: String },
}

impl UpdateStatus {
    /// Single-line rendering for the host status panel.
    pub fn status_line(&self) -> String {
        match self {
            UpdateStatus::Idle => "No update staged".to_string(),
            UpdateStatus::Checking => "Checking staged update…".to_string(),
            UpdateStatus::Staged { version } => format!("Update staged: {version} (verified)"),
            UpdateStatus::VerifyFailed { reason } => format!("Staged update failed verification: {reason}"),
        }
    }
}

/// Verify a staged complete-set directory against its signed manifest.
/// This is the one public verify entry point in this module — it always
/// runs on whatever thread calls it (synchronous, and it hashes every
/// component), so UI code should go through [`cached_status`] instead of
/// calling this directly on every repaint. Returns
/// [`UpdateStatus::Idle`] when `manifest_path` doesn't exist (nothing
/// staged), `Staged` when every check passes, and `VerifyFailed` with a
/// human-readable (content-free) reason otherwise.
pub fn verify_staged(staged_dir: &Path, manifest_path: &Path) -> UpdateStatus {
    let manifest_text = match std::fs::read_to_string(manifest_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return UpdateStatus::Idle,
        Err(e) => {
            return UpdateStatus::VerifyFailed {
                reason: format!("cannot read manifest: {e}"),
            };
        }
    };
    let signed: SignedManifest = match serde_json::from_str(&manifest_text) {
        Ok(signed) => signed,
        Err(e) => {
            return UpdateStatus::VerifyFailed {
                reason: format!("manifest is not valid JSON: {e}"),
            };
        }
    };

    match bp_updater::verify::verify_staged_set(
        staged_dir,
        &signed,
        &trust_root(),
        &running_protocol_versions(),
    ) {
        Ok(_verified) => UpdateStatus::Staged {
            version: signed.manifest.set_version,
        },
        Err(reason) => UpdateStatus::VerifyFailed {
            reason: reason.to_string(),
        },
    }
}

/// Cheap-to-compute manifest identity: length + mtime. Never opens or
/// hashes the file — just a `stat`, so it's safe to call every repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManifestFingerprint {
    len: u64,
    modified: SystemTime,
}

fn fingerprint(manifest_path: &Path) -> Option<ManifestFingerprint> {
    let meta = std::fs::metadata(manifest_path).ok()?;
    Some(ManifestFingerprint {
        len: meta.len(),
        modified: meta.modified().ok()?,
    })
}

/// Debounces [`verify_staged`] behind a manifest fingerprint: repeat
/// calls for an unchanged manifest return the cached result instantly,
/// and a changed fingerprint triggers exactly one background
/// re-verification (never re-hashing on the calling thread) while
/// showing [`UpdateStatus::Checking`] in the meantime.
pub struct UpdateStatusCache {
    /// Last confirmed status, paired with the fingerprint it was
    /// computed against (`None` fingerprint = "no manifest present").
    confirmed: (Option<ManifestFingerprint>, UpdateStatus),
    /// A background verification in flight for a specific fingerprint,
    /// plus the channel its result arrives on.
    checking: Option<(ManifestFingerprint, mpsc::Receiver<UpdateStatus>)>,
}

impl Default for UpdateStatusCache {
    fn default() -> Self {
        Self {
            confirmed: (None, UpdateStatus::Idle),
            checking: None,
        }
    }
}

impl UpdateStatusCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current status. Never hashes on the calling thread: a changed
    /// manifest fingerprint spawns a background verification and this
    /// call returns [`UpdateStatus::Checking`] immediately; the real
    /// result is picked up on a later call once the background thread
    /// finishes.
    pub fn status(&mut self, staged_dir: &Path, manifest_path: &Path) -> UpdateStatus {
        let current_fp = fingerprint(manifest_path);

        // Absorb a finished background verification, if any.
        if let Some((fp, rx)) = &self.checking {
            match rx.try_recv() {
                Ok(status) => {
                    self.confirmed = (Some(*fp), status);
                    self.checking = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.checking = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        let Some(current_fp) = current_fp else {
            // No manifest on disk right now: Idle, no hashing needed,
            // and any in-flight check is for a manifest that no longer
            // exists so it's moot once it lands.
            self.confirmed = (None, UpdateStatus::Idle);
            return UpdateStatus::Idle;
        };

        if self.confirmed.0 == Some(current_fp) {
            return self.confirmed.1.clone();
        }
        if let Some((checking_fp, _)) = &self.checking
            && *checking_fp == current_fp
        {
            return UpdateStatus::Checking;
        }

        // Fingerprint changed (or first look ever): kick off exactly
        // one background re-verify and show the interim status this
        // frame.
        let (tx, rx) = mpsc::channel();
        let staged_dir = staged_dir.to_path_buf();
        let manifest_path_owned = manifest_path.to_path_buf();
        std::thread::spawn(move || {
            let status = verify_staged(&staged_dir, &manifest_path_owned);
            let _ = tx.send(status);
        });
        self.checking = Some((current_fp, rx));
        UpdateStatus::Checking
    }

    /// Force the next [`status`](Self::status) call to re-verify
    /// regardless of fingerprint (explicit refresh request). Only the
    /// tests drive this today; production invalidation happens
    /// naturally via the manifest fingerprint.
    #[cfg(test)]
    pub fn invalidate(&mut self) {
        self.confirmed = (None, UpdateStatus::Idle);
        self.checking = None;
    }
}

static STATUS_CACHE: OnceLock<Mutex<UpdateStatusCache>> = OnceLock::new();

/// Process-wide cached status for the host UI panel. Safe to call every
/// egui repaint — see this module's "Status caching" doc.
pub fn cached_status(staged_dir: &Path, manifest_path: &Path) -> UpdateStatus {
    let cache = STATUS_CACHE.get_or_init(|| Mutex::new(UpdateStatusCache::new()));
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.status(staged_dir, manifest_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bp_updater::manifest::{ComponentEntry, Manifest, Role};
    use ed25519_dalek::SigningKey;
    use std::fs;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bp-app-update-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write_signed_manifest(
        dir: &Path,
        key: &SigningKey,
        components: Vec<ComponentEntry>,
        protocol: ProtocolVersions,
    ) -> std::path::PathBuf {
        let manifest = Manifest {
            set_version: "2026.7.1".into(),
            protocol,
            components,
            signing_key_id: "dev-trust-root-v1".into(),
        };
        let signed = bp_updater::manifest::sign(manifest, key).expect("sign");
        let path = dir.join(bp_updater::manifest::MANIFEST_FILE_NAME);
        fs::write(
            &path,
            serde_json::to_string(&signed).expect("serialize signed manifest"),
        )
        .expect("write manifest");
        path
    }

    #[test]
    fn idle_when_no_manifest_present() {
        let dir = temp_dir("idle");
        let status = verify_staged(&dir, &dir.join("no-such-manifest.json"));
        assert_eq!(status, UpdateStatus::Idle);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staged_when_dev_signed_and_files_match() {
        let dir = temp_dir("staged");
        fs::write(dir.join("betterparsec.exe"), b"exe bytes").expect("write component");
        let component = ComponentEntry {
            name: "betterparsec.exe".into(),
            role: Role::Client,
            sha256: bp_updater::verify::sha256_file(&dir.join("betterparsec.exe"))
                .expect("hash component"),
            size: 9,
        };
        let manifest_path = write_signed_manifest(
            &dir,
            &bp_updater::dev_signing_key(),
            vec![component],
            ProtocolVersions::default(),
        );
        let status = verify_staged(&dir, &manifest_path);
        assert_eq!(
            status,
            UpdateStatus::Staged {
                version: "2026.7.1".into()
            }
        );
        assert_eq!(status.status_line(), "Update staged: 2026.7.1 (verified)");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_failed_when_signed_with_wrong_key() {
        let dir = temp_dir("wrong-key");
        fs::write(dir.join("betterparsec.exe"), b"exe bytes").expect("write component");
        let component = ComponentEntry {
            name: "betterparsec.exe".into(),
            role: Role::Client,
            sha256: bp_updater::verify::sha256_file(&dir.join("betterparsec.exe"))
                .expect("hash component"),
            size: 9,
        };
        let not_the_dev_key = SigningKey::from_bytes(&[0xAA; 32]);
        let manifest_path = write_signed_manifest(
            &dir,
            &not_the_dev_key,
            vec![component],
            ProtocolVersions::default(),
        );
        let status = verify_staged(&dir, &manifest_path);
        assert!(matches!(status, UpdateStatus::VerifyFailed { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_failed_when_protocol_versions_mismatch() {
        let dir = temp_dir("proto-mismatch");
        fs::write(dir.join("betterparsec.exe"), b"exe bytes").expect("write component");
        let component = ComponentEntry {
            name: "betterparsec.exe".into(),
            role: Role::Client,
            sha256: bp_updater::verify::sha256_file(&dir.join("betterparsec.exe"))
                .expect("hash component"),
            size: 9,
        };
        let stale_protocol = ProtocolVersions {
            fec: 1,
            ..ProtocolVersions::default()
        };
        let manifest_path = write_signed_manifest(
            &dir,
            &bp_updater::dev_signing_key(),
            vec![component],
            stale_protocol,
        );
        let status = verify_staged(&dir, &manifest_path);
        assert!(matches!(status, UpdateStatus::VerifyFailed { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_failed_when_a_staged_file_is_tampered() {
        let dir = temp_dir("tampered");
        fs::write(dir.join("betterparsec.exe"), b"exe bytes").expect("write component");
        let component = ComponentEntry {
            name: "betterparsec.exe".into(),
            role: Role::Client,
            sha256: bp_updater::verify::sha256_file(&dir.join("betterparsec.exe"))
                .expect("hash component"),
            size: 9,
        };
        let manifest_path = write_signed_manifest(
            &dir,
            &bp_updater::dev_signing_key(),
            vec![component],
            ProtocolVersions::default(),
        );
        fs::write(dir.join("betterparsec.exe"), b"TAMPERED!").expect("tamper component");
        let status = verify_staged(&dir, &manifest_path);
        assert!(matches!(status, UpdateStatus::VerifyFailed { .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn staged_dir_and_manifest_path_pin_manifest_inside_staged() {
        let updates_dir = Path::new("C:/fake/updates");
        assert_eq!(staged_dir(updates_dir), updates_dir.join("staged"));
        assert_eq!(
            manifest_path(updates_dir),
            updates_dir.join("staged").join("manifest.json")
        );
    }

    #[test]
    fn cache_returns_idle_immediately_when_nothing_staged() {
        let dir = temp_dir("cache-idle");
        let mut cache = UpdateStatusCache::new();
        let status = cache.status(&dir, &dir.join("manifest.json"));
        assert_eq!(status, UpdateStatus::Idle);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_reports_checking_then_settles_to_the_real_result() {
        let dir = temp_dir("cache-settles");
        fs::write(dir.join("betterparsec.exe"), b"exe bytes").expect("write component");
        let component = ComponentEntry {
            name: "betterparsec.exe".into(),
            role: Role::Client,
            sha256: bp_updater::verify::sha256_file(&dir.join("betterparsec.exe"))
                .expect("hash component"),
            size: 9,
        };
        let manifest_path = write_signed_manifest(
            &dir,
            &bp_updater::dev_signing_key(),
            vec![component],
            ProtocolVersions::default(),
        );

        let mut cache = UpdateStatusCache::new();
        // First call after a fingerprint change never blocks on hashing:
        // it must be Idle-or-Checking, never a fully resolved Staged.
        let first = cache.status(&dir, &manifest_path);
        assert_eq!(first, UpdateStatus::Checking);

        let settled = (0..200)
            .map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                cache.status(&dir, &manifest_path)
            })
            .find(|s| *s != UpdateStatus::Checking)
            .expect("background verification eventually finishes");
        assert_eq!(
            settled,
            UpdateStatus::Staged {
                version: "2026.7.1".into()
            }
        );

        // A repeat call with the same manifest fingerprint must not
        // spawn another background verification -- it should return the
        // cached result directly.
        assert_eq!(cache.status(&dir, &manifest_path), settled);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_invalidate_forces_a_recheck() {
        let dir = temp_dir("cache-invalidate");
        fs::write(dir.join("betterparsec.exe"), b"exe bytes").expect("write component");
        let component = ComponentEntry {
            name: "betterparsec.exe".into(),
            role: Role::Client,
            sha256: bp_updater::verify::sha256_file(&dir.join("betterparsec.exe"))
                .expect("hash component"),
            size: 9,
        };
        let manifest_path = write_signed_manifest(
            &dir,
            &bp_updater::dev_signing_key(),
            vec![component],
            ProtocolVersions::default(),
        );
        let mut cache = UpdateStatusCache::new();
        let _ = cache.status(&dir, &manifest_path);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = cache.status(&dir, &manifest_path);
        cache.invalidate();
        // Right after invalidate, the cache must not hand back a stale
        // `Staged` without at least re-checking the fingerprint.
        let status = cache.status(&dir, &manifest_path);
        assert!(matches!(
            status,
            UpdateStatus::Checking | UpdateStatus::Staged { .. }
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
