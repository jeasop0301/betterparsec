//! Atomic complete-set swap: stage a verified set into the install
//! directory, keeping the previous set around for rollback. Mirrors the
//! interruption-journal pattern already used by
//! `app-native/src/identity.rs` (write a journal marker *before* each
//! risky step; any crash leaves a marker naming exactly how far the swap
//! got, and the next run — or an explicit `recover` — restores the exact
//! previous set instead of leaving a half-swapped install).
//!
//! The *decision* of what to do given a journal phase and current
//! filesystem state ([`decide_recovery`]) is a pure function, tested
//! without touching disk; the IO that carries it out
//! ([`recover_if_interrupted`], [`perform_swap`]) is exercised with real
//! temp directories.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum SwapError {
    #[error("io error during {step}: {source}")]
    Io {
        step: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("journal is corrupt: {0}")]
    CorruptJournal(String),
}

fn io_err(step: &'static str) -> impl FnOnce(std::io::Error) -> SwapError {
    move |source| SwapError::Io { step, source }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    /// The old install directory (if any) has been renamed to
    /// `prev_path`; `install` may not exist yet.
    PrevRenamed,
    /// The staged directory has been renamed into `install`; the swap is
    /// logically complete and only journal cleanup remains.
    StagedMoved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    phase: Phase,
    prev_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryAction {
    /// Rename `prev_path` back onto `install` — the swap was
    /// interrupted between renaming the old set away and moving the
    /// staged set in, so `install` is currently missing.
    RestorePrev,
    /// Nothing to undo (either the swap already completed, or there was
    /// never a previous set to restore).
    NoOp,
}

/// Pure decision: given the journal's phase and what currently exists on
/// disk, what should recovery do? No IO here — see
/// [`recover_if_interrupted`] for the side-effecting counterpart.
fn decide_recovery(phase: Phase, install_exists: bool, prev_exists: bool) -> RecoveryAction {
    match phase {
        Phase::PrevRenamed if !install_exists && prev_exists => RecoveryAction::RestorePrev,
        _ => RecoveryAction::NoOp,
    }
}

fn journal_path(install: &Path) -> PathBuf {
    let name = install.file_name().unwrap_or_default().to_string_lossy().into_owned();
    install.with_file_name(format!("{name}.update-journal.json"))
}

/// Strip path separators and other filesystem-unsafe characters out of
/// a version string before it becomes part of a filename (see
/// [`prev_path`]).
fn sanitize_version_for_filename(version: &str) -> String {
    let sanitized: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.trim_matches('_').is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

/// `<install>.prev-<version>` — the exact naming the assignment calls
/// for, tagged with the version being replaced (not the incoming one),
/// so an operator can tell at a glance what a rollback would restore.
/// `outgoing_version` is sanitized first (path separators and any other
/// non-filename-safe character become `_`) since it can come from an
/// arbitrary previously-installed manifest's `set_version`, not just a
/// trusted literal. If the sanitized candidate name is already taken on
/// disk, falls back to a timestamp-suffixed name rather than failing —
/// an operator re-running a swap after an aborted attempt should never
/// be blocked by a stale `prev` directory of the same name.
fn prev_path(install: &Path, outgoing_version: &str) -> PathBuf {
    let name = install.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let safe_version = sanitize_version_for_filename(outgoing_version);
    let candidate = install.with_file_name(format!("{name}.prev-{safe_version}"));
    if !candidate.exists() {
        return candidate;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    install.with_file_name(format!("{name}.prev-{safe_version}-{timestamp}"))
}

fn write_journal(path: &Path, journal: &Journal) -> Result<(), SwapError> {
    let text = serde_json::to_string(journal)
        .map_err(|e| SwapError::CorruptJournal(e.to_string()))?;
    fs::write(path, text).map_err(io_err("write journal"))
}

fn read_journal(path: &Path) -> Result<Journal, SwapError> {
    let text = fs::read_to_string(path).map_err(io_err("read journal"))?;
    serde_json::from_str(&text).map_err(|e| SwapError::CorruptJournal(e.to_string()))
}

fn remove_journal(path: &Path) -> Result<(), SwapError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SwapError::Io {
            step: "remove journal",
            source: e,
        }),
    }
}

/// Call before any swap attempt (and safe to call any number of times):
/// a leftover journal means a prior `swap` crashed mid-flight. Restores
/// the previous set exactly when the crash landed between "old set
/// renamed away" and "staged set moved in"; otherwise just clears the
/// journal (the swap either never started or already finished).
pub fn recover_if_interrupted(install: &Path) -> Result<(), SwapError> {
    let jpath = journal_path(install);
    if !jpath.exists() {
        return Ok(());
    }
    let journal = read_journal(&jpath)?;
    let install_exists = install.exists();
    let prev_exists = journal.prev_path.exists();
    if decide_recovery(journal.phase, install_exists, prev_exists) == RecoveryAction::RestorePrev {
        fs::rename(&journal.prev_path, install).map_err(io_err("restore previous set"))?;
    }
    remove_journal(&jpath)
}

/// Perform the atomic swap: `staged` becomes the new `install`,
/// replacing whatever was there before (kept at
/// `<install>.prev-<outgoing_version>` for manual rollback). `_verified`
/// must be a [`crate::verify::VerifiedSet`] obtained from
/// [`crate::verify::verify_staged_set`] returning `Ok` for this exact
/// `staged` directory — the type system, not just a doc comment, is
/// what stops this crate's `swap` CLI (or any other caller) from
/// installing something nobody actually verified. This function itself
/// only moves directories and manages the journal; it does not re-hash
/// or re-verify signatures.
pub fn perform_swap(
    staged: &Path,
    install: &Path,
    outgoing_version: &str,
    _verified: &crate::verify::VerifiedSet,
) -> Result<(), SwapError> {
    // Always recover first: a crash from a previous invocation must
    // never be compounded by starting a fresh swap on top of it.
    recover_if_interrupted(install)?;

    let prev = prev_path(install, outgoing_version);
    let jpath = journal_path(install);

    write_journal(
        &jpath,
        &Journal {
            phase: Phase::PrevRenamed,
            prev_path: prev.clone(),
        },
    )?;
    if install.exists() {
        // Same-filesystem rename: effectively atomic on both Windows
        // and POSIX targets for a directory move.
        fs::rename(install, &prev).map_err(io_err("rename install to prev"))?;
    }

    fs::rename(staged, install).map_err(io_err("rename staged into install"))?;

    write_journal(
        &jpath,
        &Journal {
            phase: Phase::StagedMoved,
            prev_path: prev,
        },
    )?;
    remove_journal(&jpath)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bp-updater-swap-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write_marker(dir: &Path, name: &str, contents: &str) {
        fs::create_dir_all(dir).expect("mkdir dir");
        fs::write(dir.join(name), contents).expect("write marker");
    }

    // --- pure decision-table tests (no IO) ---

    #[test]
    fn decides_restore_only_when_prev_renamed_and_install_missing_and_prev_present() {
        assert_eq!(
            decide_recovery(Phase::PrevRenamed, false, true),
            RecoveryAction::RestorePrev
        );
    }

    #[test]
    fn decides_noop_when_install_already_present() {
        assert_eq!(
            decide_recovery(Phase::PrevRenamed, true, true),
            RecoveryAction::NoOp
        );
    }

    #[test]
    fn decides_noop_when_no_prev_to_restore() {
        assert_eq!(
            decide_recovery(Phase::PrevRenamed, false, false),
            RecoveryAction::NoOp
        );
    }

    #[test]
    fn decides_noop_once_staged_already_moved() {
        assert_eq!(
            decide_recovery(Phase::StagedMoved, true, true),
            RecoveryAction::NoOp
        );
    }

    // --- IO-backed tests using real temp directories ---

    #[test]
    fn fresh_install_with_no_previous_set_succeeds() {
        let root = temp_root("fresh");
        let staged = root.join("staged");
        let install = root.join("install");
        write_marker(&staged, "betterparsec.exe", "v2");

        perform_swap(
            &staged,
            &install,
            "unknown",
            &crate::verify::VerifiedSet::assume_verified_for_test(),
        )
        .expect("swap");

        assert!(install.join("betterparsec.exe").exists());
        assert!(!staged.exists());
        assert!(!journal_path(&install).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn upgrade_over_existing_install_keeps_previous_set() {
        let root = temp_root("upgrade");
        let staged = root.join("staged");
        let install = root.join("install");
        write_marker(&install, "betterparsec.exe", "v1");
        write_marker(&staged, "betterparsec.exe", "v2");
        // Computed before the swap runs: `prev_path` returns a fresh
        // timestamp-suffixed name once a directory already occupies the
        // plain slot, so the plain slot must be captured pre-swap to
        // know where the swap will actually put the previous set.
        let prev = prev_path(&install, "1.0.0");

        perform_swap(
            &staged,
            &install,
            "1.0.0",
            &crate::verify::VerifiedSet::assume_verified_for_test(),
        )
        .expect("swap");

        assert_eq!(
            fs::read_to_string(install.join("betterparsec.exe")).expect("read installed marker"),
            "v2"
        );
        assert_eq!(
            fs::read_to_string(prev.join("betterparsec.exe")).expect("read prev marker"),
            "v1"
        );
        assert!(!journal_path(&install).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn interrupted_mid_swap_restores_the_exact_previous_set() {
        let root = temp_root("interrupted");
        let staged = root.join("staged");
        let install = root.join("install");
        let prev = prev_path(&install, "1.0.0");
        write_marker(&staged, "betterparsec.exe", "v2-partial-or-not-yet-used");
        // Simulate the exact mid-state a crash between the two renames
        // leaves behind: old install already renamed to `prev`,
        // `install` itself does not exist yet, staged is untouched.
        write_marker(&prev, "betterparsec.exe", "v1");
        write_journal(
            &journal_path(&install),
            &Journal {
                phase: Phase::PrevRenamed,
                prev_path: prev.clone(),
            },
        )
        .expect("seed journal");
        assert!(!install.exists());

        recover_if_interrupted(&install).expect("recover");

        assert_eq!(
            fs::read_to_string(install.join("betterparsec.exe")).expect("read installed marker"),
            "v1"
        );
        assert!(!prev.exists());
        assert!(!journal_path(&install).exists());
        // Staged set is untouched — a fresh swap attempt can still use it.
        assert!(staged.join("betterparsec.exe").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn interrupted_after_staged_move_just_clears_the_journal() {
        let root = temp_root("post-move");
        let install = root.join("install");
        write_marker(&install, "betterparsec.exe", "v2");
        let prev = prev_path(&install, "1.0.0");
        write_marker(&prev, "betterparsec.exe", "v1");
        write_journal(
            &journal_path(&install),
            &Journal {
                phase: Phase::StagedMoved,
                prev_path: prev.clone(),
            },
        )
        .expect("seed journal");

        recover_if_interrupted(&install).expect("recover");

        // Already-completed swap is left exactly as-is: new set
        // installed, previous set kept for manual rollback.
        assert_eq!(
            fs::read_to_string(install.join("betterparsec.exe")).expect("read installed marker"),
            "v2"
        );
        assert!(prev.exists());
        assert!(!journal_path(&install).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_is_idempotent_when_no_journal_present() {
        let root = temp_root("idempotent-recover");
        let install = root.join("install");
        write_marker(&install, "betterparsec.exe", "v1");
        recover_if_interrupted(&install).expect("first recover no-op");
        recover_if_interrupted(&install).expect("second recover no-op");
        assert!(install.join("betterparsec.exe").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prev_path_sanitizes_path_separators_and_traversal_in_the_version_string() {
        let root = temp_root("prev-sanitize");
        let install = root.join("install");
        let resolved = prev_path(&install, "../../evil/1.0.0");
        let file_name = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .expect("prev dir has a file name")
            .to_string();
        assert!(!file_name.contains('/'));
        assert!(!file_name.contains('\\'));
        // The literal ".." segment must not survive as its own path
        // component (it is always embedded inside a longer
        // "<name>.prev-..." filename, never standing alone).
        assert_ne!(file_name, "..");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prev_path_falls_back_to_a_timestamped_name_when_the_plain_one_already_exists() {
        let root = temp_root("prev-collision");
        let install = root.join("install");
        let candidate = install.with_file_name("install.prev-1.0.0");
        fs::create_dir_all(&candidate).expect("seed collision dir");

        let resolved = prev_path(&install, "1.0.0");

        assert_ne!(resolved, candidate);
        assert!(!resolved.exists());
        let resolved_name = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .expect("file name");
        assert!(resolved_name.starts_with("install.prev-1.0.0-"));
        let _ = fs::remove_dir_all(&root);
    }
}
