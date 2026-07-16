//! G005 identity migration — backup-first move of the pre-managed
//! Sunshine identity (certs/state/apps, `sunshine.rs::SunshineConfig`'s
//! `identity_source`) and the standalone server's account/pairing config
//! (`host.rs::DEFAULT_CONFIG_PATH`) into the managed layout
//! (`settings::HostSettings.paths`).
//!
//! Contract (owner: "never lose a paired install"):
//! (a) snapshot/back up the complete prior destination state *before*
//!     touching anything;
//! (b) copy source bytes verbatim into a staging area — never
//!     regenerate/reserialize a file we don't own the format of;
//! (c) verify every staged file against its source by content hash
//!     *and* parse-validate every parseable (JSON) file;
//! (d) atomically switch (rename) the verified staging area over the
//!     destination only after (c) passes;
//! (e) roll back to the last-known-good backup on any failure, including
//!     a prior run that crashed mid-migration (detected via a journal
//!     file left behind — see [`recover_on_start`]).
//!
//! The backup is never deleted on success — it is the last-known-good
//! copy, kept for the life of the install so a bad migration is always
//! recoverable even after the app has since restarted cleanly.
//!
//! Never logs file contents/bytes (only paths and counts) — secrets
//! (certs/keys/pairing tokens) stay out of the trace log; redaction of
//! anything that *does* need to be logged is the supervision lane's job
//! (`103-HostSupervisionCore`), not this module's.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Source (pre-G005 unmanaged) locations and the managed destination
/// root to migrate into. `dest_root` is `HostSettings.paths`'s shared
/// parent (i.e. `<data_dir>/host`): identity lands at
/// `dest_root/identity`, the server config at `dest_root/config.json` —
/// matching `HostPaths::sunshine_identity_dir` / `HostPaths::config_path`
/// exactly, so callers can build this straight from `Settings`.
#[derive(Debug, Clone)]
pub struct MigrationPaths {
    /// Old Sunshine identity dir (certs/state/apps) — may not exist on a
    /// fresh install that never ran unmanaged Sunshine.
    pub source_identity_dir: PathBuf,
    /// Old standalone server config (`host.rs::DEFAULT_CONFIG_PATH`) —
    /// may not exist on a fresh install.
    pub source_server_config: PathBuf,
    /// Managed root; identity/config land at
    /// `dest_root/identity` and `dest_root/config.json`.
    pub dest_root: PathBuf,
}

impl MigrationPaths {
    pub fn dest_identity_dir(&self) -> PathBuf {
        self.dest_root.join("identity")
    }

    pub fn dest_server_config(&self) -> PathBuf {
        self.dest_root.join("config.json")
    }
}

/// Result of a completed (non-no-op) migration — counts only, never
/// content, so a caller can log it safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationReport {
    pub files_migrated: usize,
}

fn journal_path(dest_root: &Path) -> PathBuf {
    dest_root.join(".migration-journal.json")
}

fn backup_dir(dest_root: &Path) -> PathBuf {
    dest_root.join(".backup-lkg")
}

fn staging_dir(dest_root: &Path) -> PathBuf {
    dest_root.join(".staging")
}

/// Interruption-journal phase — written before each step so a crash
/// between steps leaves a marker naming exactly how far the migration
/// got. `recover_on_start` treats *any* journal presence as "not done,
/// roll back" regardless of phase: only the phase-less, journal-absent
/// state is "safely idle".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    BackedUp,
    Staged,
    Verified,
}

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    phase: Phase,
}

fn write_journal(dest_root: &Path, phase: Phase) -> Result<(), String> {
    std::fs::create_dir_all(dest_root)
        .map_err(|e| format!("mkdir {}: {e}", dest_root.display()))?;
    let text = serde_json::to_string(&Journal { phase }).map_err(|e| e.to_string())?;
    std::fs::write(journal_path(dest_root), text).map_err(|e| e.to_string())
}

fn remove_journal(dest_root: &Path) -> Result<(), String> {
    match std::fs::remove_file(journal_path(dest_root)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Remove a file or directory tree; already-absent is not an error (idle
/// cleanup, not a report of prior state).
fn remove_path_any(path: &Path) -> Result<(), String> {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else if path.exists() {
        std::fs::remove_file(path)
    } else {
        return Ok(());
    };
    result.map_err(|e| format!("remove {}: {e}", path.display()))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("read_dir {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = dst.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Non-cryptographic content hash — sufficient for a same-machine
/// copy-integrity check (not a security boundary; nothing here is
/// exposed to an adversary who couldn't just read the file directly).
fn hash_file(path: &Path) -> Result<u64, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(hasher.finish())
}

/// (c) verify one file: content hash equality against its source, plus
/// parse-validation when the extension says it's JSON (state/apps/server
/// config — `.conf` files are key=value text, not JSON, so skipped).
fn verify_file_matches(source: &Path, staged: &Path) -> Result<(), String> {
    let src_hash = hash_file(source)?;
    let dst_hash = hash_file(staged)?;
    if src_hash != dst_hash {
        return Err(format!(
            "content hash mismatch after copy: {} vs {}",
            source.display(),
            staged.display()
        ));
    }
    if source.extension().and_then(|e| e.to_str()) == Some("json") {
        let bytes = std::fs::read(staged).map_err(|e| e.to_string())?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| format!("staged {} failed parse-validation: {e}", staged.display()))?;
    }
    Ok(())
}

/// Verify every file under `source` has a byte-identical, parse-valid
/// counterpart at the same relative path under `staged`. Returns the
/// verified file count.
fn verify_dir_matches(source: &Path, staged: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let src_dir = source.join(&rel);
        for entry in std::fs::read_dir(&src_dir)
            .map_err(|e| format!("read_dir {}: {e}", src_dir.display()))?
        {
            let entry = entry.map_err(|e| e.to_string())?;
            let rel_child = rel.join(entry.file_name());
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                stack.push(rel_child);
            } else {
                let staged_file = staged.join(&rel_child);
                if !staged_file.is_file() {
                    return Err(format!("staged copy missing {}", rel_child.display()));
                }
                verify_file_matches(&entry.path(), &staged_file)?;
                count += 1;
            }
        }
    }
    Ok(count)
}

/// (e) Restore the destination to the last-known-good backup (or clear a
/// partial destination that never had a confirmed prior state), then
/// drop the interruption journal and any leftover staging area. Safe to
/// call any number of times — every step tolerates its target already
/// being absent.
pub fn rollback(paths: &MigrationPaths) -> Result<(), String> {
    let dest_identity = paths.dest_identity_dir();
    let dest_config = paths.dest_server_config();
    let backup = backup_dir(&paths.dest_root);
    let backup_identity = backup.join("identity");
    let backup_config = backup.join("config.json");

    remove_path_any(&staging_dir(&paths.dest_root))?;
    remove_path_any(&dest_identity)?;
    remove_path_any(&dest_config)?;

    if backup_identity.is_dir() {
        copy_dir_recursive(&backup_identity, &dest_identity)?;
    }
    if backup_config.is_file() {
        std::fs::create_dir_all(&paths.dest_root)
            .map_err(|e| format!("mkdir {}: {e}", paths.dest_root.display()))?;
        std::fs::copy(&backup_config, &dest_config).map_err(|e| e.to_string())?;
    }

    remove_journal(&paths.dest_root)?;
    Ok(())
}

/// Call once at process start, before any other read of the managed
/// layout: a leftover journal means a prior migration crashed between
/// steps (b)/(c)/(d) and never reached the "journal removed" success
/// state, so the destination might be half-switched. Roll it back to the
/// last-known-good backup unconditionally — the app must never boot on
/// a half-migrated identity. Returns whether a rollback actually ran.
pub fn recover_on_start(paths: &MigrationPaths) -> Result<bool, String> {
    if journal_path(&paths.dest_root).exists() {
        rollback(paths)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Run the backup-first migration. A no-op (returns `Ok` with 0 files)
/// when neither source exists — a fresh managed install with no legacy
/// state to migrate. On any failure, best-effort rolls the destination
/// back immediately (in addition to the journal protecting a genuine
/// crash on the *next* start).
pub fn migrate(paths: &MigrationPaths) -> Result<MigrationReport, String> {
    recover_on_start(paths)?;

    if !paths.source_identity_dir.is_dir() && !paths.source_server_config.is_file() {
        return Ok(MigrationReport { files_migrated: 0 });
    }

    match run_migration(paths) {
        Ok(report) => Ok(report),
        Err(e) => {
            // The journal still protects the next start if this rollback
            // itself fails, but a rollback failure must not vanish: it means
            // the destination may be mid-switch right now.
            if let Err(rb) = rollback(paths) {
                tracing::error!(error = %rb, "identity migration rollback failed after error");
            }
            Err(e)
        }
    }
}

fn run_migration(paths: &MigrationPaths) -> Result<MigrationReport, String> {
    let dest_root = &paths.dest_root;
    std::fs::create_dir_all(dest_root)
        .map_err(|e| format!("mkdir {}: {e}", dest_root.display()))?;

    let dest_identity = paths.dest_identity_dir();
    let dest_config = paths.dest_server_config();

    // (a) snapshot the complete prior destination state before writing
    // anything — this is what `rollback` restores on any failure.
    let backup = backup_dir(dest_root);
    remove_path_any(&backup)?;
    if dest_identity.is_dir() {
        copy_dir_recursive(&dest_identity, &backup.join("identity"))?;
    }
    if dest_config.is_file() {
        std::fs::create_dir_all(&backup).map_err(|e| format!("mkdir {}: {e}", backup.display()))?;
        std::fs::copy(&dest_config, backup.join("config.json")).map_err(|e| e.to_string())?;
    }
    write_journal(dest_root, Phase::BackedUp)?;

    // (b) copy source bytes into staging, verbatim — no regeneration.
    let staging = staging_dir(dest_root);
    remove_path_any(&staging)?;
    let staged_identity = staging.join("identity");
    let staged_config = staging.join("config.json");
    if paths.source_identity_dir.is_dir() {
        copy_dir_recursive(&paths.source_identity_dir, &staged_identity)?;
    }
    if paths.source_server_config.is_file() {
        std::fs::create_dir_all(&staging)
            .map_err(|e| format!("mkdir {}: {e}", staging.display()))?;
        std::fs::copy(&paths.source_server_config, &staged_config).map_err(|e| e.to_string())?;
    }
    write_journal(dest_root, Phase::Staged)?;

    // (c) verify: content hash equality + parse-validation.
    let mut files_migrated = 0usize;
    if paths.source_identity_dir.is_dir() {
        files_migrated += verify_dir_matches(&paths.source_identity_dir, &staged_identity)?;
    }
    if paths.source_server_config.is_file() {
        verify_file_matches(&paths.source_server_config, &staged_config)?;
        files_migrated += 1;
    }
    write_journal(dest_root, Phase::Verified)?;

    // (d) atomic switch: only after verification passes, rename the
    // verified staging content over the (already backed-up) destination.
    remove_path_any(&dest_identity)?;
    remove_path_any(&dest_config)?;
    if staged_identity.is_dir() {
        std::fs::rename(&staged_identity, &dest_identity)
            .map_err(|e| format!("switch identity dir: {e}"))?;
    }
    if staged_config.is_file() {
        std::fs::rename(&staged_config, &dest_config)
            .map_err(|e| format!("switch server config: {e}"))?;
    }

    remove_path_any(&staging)?;
    remove_journal(dest_root)?;
    // Never remove `backup`: it is the last-known-good copy, retained
    // for the life of the install even after a clean success.

    tracing::info!(
        files_migrated,
        dest = %dest_root.display(),
        "identity migration complete"
    );
    Ok(MigrationReport { files_migrated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bp-identity-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn identity_fixture(dir: &Path, tag: &str) {
        std::fs::create_dir_all(dir.join("credentials")).expect("mkdir credentials");
        std::fs::write(dir.join("sunshine.conf"), format!("# conf {tag}")).expect("conf");
        std::fs::write(
            dir.join("sunshine_state.json"),
            format!("{{\"tag\":\"{tag}\"}}"),
        )
        .expect("state");
        std::fs::write(dir.join("apps.json"), "[]").expect("apps");
        std::fs::write(
            dir.join("credentials").join("cakey.pem"),
            format!("key-{tag}"),
        )
        .expect("key");
        std::fs::write(
            dir.join("credentials").join("cacert.pem"),
            format!("cert-{tag}"),
        )
        .expect("cert");
    }

    fn server_config_fixture(path: &Path, tag: &str) {
        std::fs::write(path, format!("{{\"server_tag\":\"{tag}\"}}")).expect("server config");
    }

    fn read_all_bytes(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let d = dir.join(&rel);
            for entry in std::fs::read_dir(&d).expect("read_dir") {
                let entry = entry.expect("entry");
                let rel_child = rel.join(entry.file_name());
                if entry.file_type().expect("file_type").is_dir() {
                    stack.push(rel_child);
                } else {
                    let bytes = std::fs::read(entry.path()).expect("read file");
                    out.push((rel_child, bytes));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    #[test]
    fn no_op_when_nothing_to_migrate() {
        let root = temp_dir("noop");
        let paths = MigrationPaths {
            source_identity_dir: root.join("no-such-identity"),
            source_server_config: root.join("no-such-config.json"),
            dest_root: root.join("dest"),
        };
        let report = migrate(&paths).expect("no-op migrate");
        assert_eq!(report.files_migrated, 0);
        assert!(!paths.dest_identity_dir().exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_copies_bytes_identically_and_cleans_up() {
        let root = temp_dir("fresh");
        let source_identity = root.join("source-identity");
        identity_fixture(&source_identity, "v1");
        let source_config = root.join("source-config.json");
        server_config_fixture(&source_config, "v1");

        let dest_root = root.join("dest");
        let paths = MigrationPaths {
            source_identity_dir: source_identity.clone(),
            source_server_config: source_config.clone(),
            dest_root: dest_root.clone(),
        };

        let report = migrate(&paths).expect("migrate");
        assert_eq!(report.files_migrated, 6); // 5 identity files + 1 server config

        // (identity byte-identity) — every migrated file hashes/matches
        // the source exactly, no regeneration in between.
        let source_files = read_all_bytes(&source_identity);
        let dest_files = read_all_bytes(&paths.dest_identity_dir());
        assert_eq!(source_files, dest_files);
        assert_eq!(
            std::fs::read(&source_config).expect("read source config"),
            std::fs::read(paths.dest_server_config()).expect("read dest config")
        );

        // No leftover journal/staging after a clean success.
        assert!(!journal_path(&dest_root).exists());
        assert!(!staging_dir(&dest_root).exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn interrupted_migration_rolls_back_to_last_known_good_on_next_start() {
        let root = temp_dir("interrupted");
        let dest_root = root.join("dest");

        // First, get a fully-migrated dest in place (v1) — this becomes
        // the "prior state" a second migration must be able to restore.
        let source_v1 = root.join("source-v1");
        identity_fixture(&source_v1, "v1");
        let config_v1 = root.join("config-v1.json");
        server_config_fixture(&config_v1, "v1");
        let paths = MigrationPaths {
            source_identity_dir: source_v1.clone(),
            source_server_config: config_v1.clone(),
            dest_root: dest_root.clone(),
        };
        migrate(&paths).expect("first migration");
        let good_identity = read_all_bytes(&paths.dest_identity_dir());
        let good_config = std::fs::read(paths.dest_server_config()).expect("read v1 dest config");

        // Now simulate a second migration (v2 source) that crashes right
        // after staging — drop the journal mid-state: backup taken
        // (holds v1), staging has v2 bytes, but the switch never
        // happened and the process died before cleanup.
        let source_v2 = root.join("source-v2");
        identity_fixture(&source_v2, "v2");
        let config_v2 = root.join("config-v2.json");
        server_config_fixture(&config_v2, "v2");
        let paths_v2 = MigrationPaths {
            source_identity_dir: source_v2.clone(),
            source_server_config: config_v2.clone(),
            dest_root: dest_root.clone(),
        };

        // Manually reproduce the mid-run on-disk state instead of racing
        // a real crash: backup already holds the pre-run (v1) dest,
        // staging holds the freshly-copied v2 source, and a journal
        // marks the run as having gotten to "Staged" — exactly what a
        // process that died between steps (b) and (d) leaves behind.
        remove_path_any(&backup_dir(&dest_root)).expect("clear backup");
        copy_dir_recursive(
            &paths.dest_identity_dir(),
            &backup_dir(&dest_root).join("identity"),
        )
        .expect("snapshot v1 identity into backup");
        std::fs::copy(
            paths.dest_server_config(),
            backup_dir(&dest_root).join("config.json"),
        )
        .expect("snapshot v1 config into backup");
        copy_dir_recursive(&source_v2, &staging_dir(&dest_root).join("identity"))
            .expect("stage v2 identity");
        std::fs::copy(&config_v2, staging_dir(&dest_root).join("config.json"))
            .expect("stage v2 config");
        write_journal(&dest_root, Phase::Staged).expect("write interruption marker");

        // Next start: recover_on_start (called at the top of `migrate`,
        // and directly here) must roll dest back to v1, not leave it
        // half-switched and not silently keep the abandoned v2 staging.
        let rolled_back = recover_on_start(&paths_v2).expect("recover");
        assert!(rolled_back, "a leftover journal must trigger a rollback");

        assert!(!journal_path(&dest_root).exists());
        assert!(!staging_dir(&dest_root).exists());
        let restored_identity = read_all_bytes(&paths_v2.dest_identity_dir());
        assert_eq!(
            restored_identity, good_identity,
            "rollback must restore the complete prior (v1) state, not the interrupted v2 copy"
        );
        assert_eq!(
            std::fs::read(paths_v2.dest_server_config()).expect("read restored config"),
            good_config
        );

        // The backup itself must survive the rollback (never deleted).
        assert!(backup_dir(&dest_root).join("identity").is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn successful_migration_keeps_backup_of_prior_state() {
        let root = temp_dir("keep-backup");
        let dest_root = root.join("dest");

        let source_v1 = root.join("source-v1");
        identity_fixture(&source_v1, "v1");
        let config_v1 = root.join("config-v1.json");
        server_config_fixture(&config_v1, "v1");
        let paths_v1 = MigrationPaths {
            source_identity_dir: source_v1,
            source_server_config: config_v1,
            dest_root: dest_root.clone(),
        };
        migrate(&paths_v1).expect("first migration");
        // No prior dest state existed yet, so there is nothing to back
        // up on this first run.
        assert!(!backup_dir(&dest_root).exists());

        // A second migration onto an already-populated dest must back
        // that prior (now v1) state up before switching to v2, and must
        // keep the backup afterward (never deleted on success).
        let source_v2 = root.join("source-v2");
        identity_fixture(&source_v2, "v2");
        let config_v2 = root.join("config-v2.json");
        server_config_fixture(&config_v2, "v2");
        let paths_v2 = MigrationPaths {
            source_identity_dir: source_v2,
            source_server_config: config_v2,
            dest_root: dest_root.clone(),
        };
        migrate(&paths_v2).expect("second migration");

        assert!(
            backup_dir(&dest_root).join("identity").is_dir(),
            "backup of the pre-second-migration (v1) state must be retained"
        );
        let backed_up_state = std::fs::read_to_string(
            backup_dir(&dest_root)
                .join("identity")
                .join("sunshine.conf"),
        )
        .expect("read backed up conf");
        assert_eq!(backed_up_state, "# conf v1");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_source_json_fails_verification_and_rolls_back() {
        // Parse-validation guard: a byte-identical-but-corrupt JSON file
        // must fail migration rather than switching in an unparseable
        // state file — even though the hash-equality check alone
        // wouldn't catch it (source and staged copy are byte-identical
        // to *each other*, just both broken).
        let root = temp_dir("corrupt-source");
        let source_identity = root.join("source-identity");
        std::fs::create_dir_all(source_identity.join("credentials")).expect("mkdir");
        std::fs::write(source_identity.join("sunshine.conf"), "# conf").expect("conf");
        std::fs::write(source_identity.join("sunshine_state.json"), "{ not json").expect("state");
        std::fs::write(source_identity.join("apps.json"), "[]").expect("apps");
        std::fs::write(source_identity.join("credentials").join("cakey.pem"), "key").expect("key");
        std::fs::write(
            source_identity.join("credentials").join("cacert.pem"),
            "cert",
        )
        .expect("cert");

        let dest_root = root.join("dest");
        let paths = MigrationPaths {
            source_identity_dir: source_identity,
            source_server_config: root.join("no-such-config.json"),
            dest_root: dest_root.clone(),
        };

        let result = migrate(&paths);
        assert!(result.is_err(), "corrupt state.json must fail migration");
        assert!(
            !paths.dest_identity_dir().exists(),
            "dest must not be switched in on failure"
        );
        assert!(
            !journal_path(&dest_root).exists(),
            "failure path must clean up its journal"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
