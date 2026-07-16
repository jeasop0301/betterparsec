//! Foundation Sunshine managed subprocess — A1 slice 3 (design D2
//! Phase A, docs/design/unified-app-architecture.md §2).
//!
//! Rust port of the launch contract proven live by
//! `tools/benchmark/host/Start-FoundationPaired.ps1` (4 paired
//! sessions, f1-ack.md step 4): staged `sunshine.exe` + a fresh
//! per-run live config directory (identity — certs/state/apps —
//! copied from a source config so pairing survives) + explicit
//! path/port overrides on the command line.
//!
//! Scope: the unified app manages **only its staged binary**. The
//! benchmark scripts' stock-`SunshineService` swap, hash verification,
//! and watchdog stay script-side — they exist to protect a stock
//! install during tests; the product host role never touches services.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// How to launch the staged Foundation Sunshine.
#[derive(Debug, Clone)]
pub struct SunshineConfig {
    /// Directory containing the staged Foundation `sunshine(.exe)`.
    pub stage_root: PathBuf,
    /// Config directory copied into the fresh live dir (pairing
    /// identity: credentials, state, apps). The benchmark scripts use
    /// the stock install's config; the unified app may point anywhere.
    pub identity_source: PathBuf,
    /// Moonlight HTTP port — must equal the embedded web-server's
    /// `moonlight.default_http_port` so pairing finds the host.
    pub port: u16,
    /// Additional `key=value` launch overrides appended after the
    /// fixed contract args above (`launch_args`) — the settings-derived
    /// child-config values (`settings::generate_child_config`, parsed by
    /// `host.rs`'s `extra_launch_args_from_child_config`) so the
    /// generated child config file actually drives the launched process
    /// instead of sitting unread on disk. Empty in callers/tests that
    /// don't care (e.g. the stub-exe tests below).
    pub extra_args: Vec<String>,
}

/// Everything path-shaped about one live run (pure derivation — tested).
#[derive(Debug, PartialEq, Eq)]
pub struct LivePaths {
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub private_key: PathBuf,
    pub certificate: PathBuf,
    pub state_file: PathBuf,
    pub apps_file: PathBuf,
    pub sunshine_log: PathBuf,
    /// Retained for the launch-contract's live-dir layout; no longer
    /// file-backed — stdout is consolidated into the host log via
    /// `supervisor::spawn_log_pump`'s redaction pass instead (see
    /// `SunshineProcess::spawn`).
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
}

fn live_paths(stage_root: &Path, stamp: &str) -> LivePaths {
    let root = stage_root.join(format!("paired-live-{stamp}"));
    let config_dir = root.join("config");
    LivePaths {
        config_file: config_dir.join("sunshine.conf"),
        private_key: config_dir.join("credentials").join("cakey.pem"),
        certificate: config_dir.join("credentials").join("cacert.pem"),
        state_file: config_dir.join("sunshine_state.json"),
        apps_file: config_dir.join("apps.json"),
        sunshine_log: config_dir.join("foundation.sunshine.log"),
        stdout_log: root.join("foundation.stdout.log"),
        stderr_log: root.join("foundation.stderr.log"),
        config_dir,
        root,
    }
}

/// The exact argument contract Start-FoundationPaired.ps1 passes (order
/// included): positional config file, then `key=value` overrides.
fn launch_args(paths: &LivePaths, port: u16, extra: &[String]) -> Vec<String> {
    let mut args = vec![
        paths.config_file.to_string_lossy().into_owned(),
        format!("port={port}"),
        "upnp=disabled".into(),
        format!("pkey={}", paths.private_key.to_string_lossy()),
        format!("cert={}", paths.certificate.to_string_lossy()),
        format!("file_state={}", paths.state_file.to_string_lossy()),
        format!("credentials_file={}", paths.state_file.to_string_lossy()),
        format!("file_apps={}", paths.apps_file.to_string_lossy()),
        format!("log_path={}", paths.sunshine_log.to_string_lossy()),
    ];
    args.extend(extra.iter().cloned());
    args
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn tail_of(path: &Path, max: usize) -> String {
    let mut text = String::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        let _ = f.read_to_string(&mut text);
    }
    let start = text.len().saturating_sub(max);
    text[start..].trim().to_string()
}

/// A managed staged-Foundation process bound to one live config dir.
#[derive(Debug)]
pub struct SunshineProcess {
    child: std::process::Child,
    pub paths: LivePaths,
    pub port: u16,
    /// Join handle for the redacting stderr tee thread (see
    /// `supervisor::spawn_redacting_tee`) — joined by `wait_ready`'s
    /// startup-death path before it reads `tail_of(&paths.stderr_log)`,
    /// so the on-disk tail it reports has actually been fully drained
    /// and redacted (no partial-write race against the tee thread).
    stderr_pump: Option<std::thread::JoinHandle<()>>,
}

impl SunshineProcess {
    /// Prepare a fresh live dir (identity copy) and launch the staged
    /// binary. Fails early when the exe/identity are missing or the
    /// moonlight port is already owned by someone else.
    pub fn spawn(cfg: &SunshineConfig) -> Result<Self, String> {
        let exe = cfg
            .stage_root
            .join(format!("sunshine{}", std::env::consts::EXE_SUFFIX));
        if !exe.is_file() {
            return Err(format!("staged sunshine not found: {}", exe.display()));
        }
        if !cfg.identity_source.is_dir() {
            return Err(format!(
                "identity source not found: {}",
                cfg.identity_source.display()
            ));
        }
        // Same pre-check as the script's Get-NetTCPConnection guard: a
        // successful bind proves nobody owns the moonlight port yet.
        match std::net::TcpListener::bind(("0.0.0.0", cfg.port)) {
            Ok(l) => drop(l),
            Err(e) => {
                return Err(format!(
                    "moonlight port {} is already in use: {e}",
                    cfg.port
                ));
            }
        }

        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let paths = live_paths(&cfg.stage_root, &stamp);
        copy_dir_recursive(&cfg.identity_source, &paths.config_dir)
            .map_err(|e| format!("copy identity into live config: {e}"))?;

        // stdout and stderr are both piped and consolidated into the
        // host log via the G005 supervision lane's redaction pass
        // (`supervisor::spawn_log_pump` / `supervisor::
        // spawn_redacting_tee`) instead of a raw file redirect — the
        // stderr tee additionally re-writes the redacted lines to
        // `paths.stderr_log` so `wait_ready`'s startup-failure
        // diagnostic (`tail_of`) never surfaces a raw secret either on
        // disk or in the returned error string (the latter is also
        // explicitly `redact`ed as belt-and-braces — see `wait_ready`).
        let mut child = std::process::Command::new(&exe)
            .args(launch_args(&paths, cfg.port, &cfg.extra_args))
            .current_dir(&cfg.stage_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("launch {}: {e}", exe.display()))?;
        if let Some(stdout) = child.stdout.take() {
            crate::supervisor::spawn_log_pump(stdout, "foundation-stdout");
        }
        let stderr_pump = child.stderr.take().and_then(|stderr| {
            crate::supervisor::spawn_redacting_tee(stderr, paths.stderr_log.clone())
        });
        tracing::info!(pid = child.id(), port = cfg.port, live = %paths.root.display(),
            "foundation sunshine launched");
        Ok(Self {
            child,
            paths,
            port: cfg.port,
            stderr_pump,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Block until the moonlight HTTP port accepts connections (the
    /// script's PID-owns-port check, observed from the outside) or the
    /// process exits — whichever comes first, bounded by `timeout`.
    pub fn wait_ready(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                // Join the stderr tee so the file it writes to is fully
                // drained/redacted before we read it — otherwise this
                // could race the tee thread and read a partial tail.
                // `redact` is also applied to the joined result directly
                // (belt-and-braces: it's already redacted on write, but
                // this keeps the invariant "no raw stderr byte reaches
                // `last_error`/the log/the UI" true even if the on-disk
                // write path ever changes).
                if let Some(handle) = self.stderr_pump.take() {
                    let _ = handle.join();
                }
                return Err(format!(
                    "sunshine exited during startup ({status}); stderr: {}",
                    crate::supervisor::redact(&tail_of(&self.paths.stderr_log, 512))
                ));
            }
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.port));
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "sunshine did not accept on port {} within {:?}",
                    self.port, timeout
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The raw child handle — used by the G005 supervisor to assign
    /// this process into the app-wide Job Object (containment) without
    /// `sunshine.rs` knowing about job objects.
    pub fn raw_child(&self) -> &std::process::Child {
        &self.child
    }

    /// Best-effort exit code once the process has actually exited;
    /// `None` while still running or if the platform can't report a
    /// code. Never blocks (`try_wait`, not `wait`).
    pub fn try_exit_code(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }

    /// Kill (if still alive) and reap. Killing an exited process is not
    /// an error.
    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        tracing::info!("foundation sunshine stopped");
    }
}

// ── Tests (headless — a stub exe stands in for Foundation) ────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bp-sun-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Minimal identity source: the files the live run overrides.
    fn identity_fixture(dir: &Path) {
        std::fs::create_dir_all(dir.join("credentials")).expect("mkdir credentials");
        std::fs::write(dir.join("sunshine.conf"), "# conf").expect("conf");
        std::fs::write(dir.join("sunshine_state.json"), "{}").expect("state");
        std::fs::write(dir.join("apps.json"), "{}").expect("apps");
        std::fs::write(dir.join("credentials").join("cakey.pem"), "key").expect("key");
        std::fs::write(dir.join("credentials").join("cacert.pem"), "cert").expect("cert");
    }

    /// A stub `sunshine.exe` that exits immediately (cmd.exe copy on
    /// Windows) — exercises spawn + the startup-death path.
    fn stub_stage(tag: &str) -> PathBuf {
        let stage = temp_dir(tag);
        let stub = if cfg!(windows) {
            std::env::var_os("ComSpec").expect("ComSpec")
        } else {
            "/bin/true".into()
        };
        std::fs::copy(
            &stub,
            stage.join(format!("sunshine{}", std::env::consts::EXE_SUFFIX)),
        )
        .expect("stage stub");
        stage
    }

    #[test]
    fn launch_args_match_the_script_contract() {
        let paths = live_paths(Path::new("stage"), "S");
        let args = launch_args(&paths, 49_000, &[]);
        let cfg = Path::new("stage").join("paired-live-S").join("config");
        assert_eq!(args[0], cfg.join("sunshine.conf").to_string_lossy());
        assert_eq!(args[1], "port=49000");
        assert_eq!(args[2], "upnp=disabled");
        assert_eq!(
            args[3],
            format!(
                "pkey={}",
                cfg.join("credentials").join("cakey.pem").to_string_lossy()
            )
        );
        assert_eq!(
            args[5],
            format!(
                "file_state={}",
                cfg.join("sunshine_state.json").to_string_lossy()
            )
        );
        // credentials_file intentionally aliases file_state (script parity).
        assert_eq!(
            args[6],
            format!(
                "credentials_file={}",
                cfg.join("sunshine_state.json").to_string_lossy()
            )
        );
        assert_eq!(args.len(), 9);
    }

    #[test]
    fn launch_args_append_extra_overrides_after_the_contract_args() {
        let paths = live_paths(Path::new("stage"), "S");
        let extra = vec!["bitrate_kbps=6000".to_string(), "fps=60".to_string()];
        let args = launch_args(&paths, 49_000, &extra);
        assert_eq!(args.len(), 11, "9 contract args + 2 extra overrides");
        assert_eq!(args[9], "bitrate_kbps=6000");
        assert_eq!(args[10], "fps=60");
    }

    #[test]
    fn spawn_copies_identity_into_a_fresh_live_dir() {
        let stage = stub_stage("live");
        let identity = temp_dir("identity");
        identity_fixture(&identity);

        let proc = SunshineProcess::spawn(&SunshineConfig {
            stage_root: stage.clone(),
            identity_source: identity.clone(),
            port: 0, // bind pre-check: port 0 always bindable
            extra_args: Vec::new(),
        })
        .expect("spawn stub");
        assert!(proc.paths.config_file.is_file(), "conf copied");
        assert!(proc.paths.private_key.is_file(), "identity key copied");
        assert!(proc.paths.state_file.is_file(), "state copied");
        proc.stop();
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_dir_all(&identity);
    }

    #[test]
    fn startup_death_surfaces_with_stderr_context() {
        let stage = stub_stage("death");
        let identity = temp_dir("identity2");
        identity_fixture(&identity);

        // The stub exits immediately: wait_ready must report the exit
        // instead of spinning until the timeout.
        let mut proc = SunshineProcess::spawn(&SunshineConfig {
            stage_root: stage.clone(),
            identity_source: identity.clone(),
            port: 0,
            extra_args: Vec::new(),
        })
        .expect("spawn stub");
        let err = proc
            .wait_ready(Duration::from_secs(10))
            .expect_err("stub cannot become ready");
        assert!(
            err.contains("exited during startup"),
            "death path reported: {err}"
        );
        assert!(proc.paths.stderr_log.is_file(), "stderr redirected");
        proc.stop(); // stopping an exited process is not an error
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_dir_all(&identity);
    }

    #[test]
    fn missing_exe_and_busy_port_fail_early() {
        let empty = temp_dir("noexe");
        let identity = temp_dir("identity3");
        identity_fixture(&identity);
        let err = SunshineProcess::spawn(&SunshineConfig {
            stage_root: empty.clone(),
            identity_source: identity.clone(),
            port: 0,
            extra_args: Vec::new(),
        })
        .expect_err("no exe");
        assert!(err.contains("staged sunshine not found"), "{err}");

        // Occupy a port; the pre-check must refuse before launching.
        let stage = stub_stage("busy");
        let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let err = SunshineProcess::spawn(&SunshineConfig {
            stage_root: stage.clone(),
            identity_source: identity.clone(),
            port,
            extra_args: Vec::new(),
        })
        .expect_err("port busy");
        assert!(err.contains("already in use"), "{err}");
        drop(listener);
        let _ = std::fs::remove_dir_all(&empty);
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_dir_all(&identity);
    }

    /// End-to-end proof of the G005 redaction invariant across the
    /// real `spawn`→pipe→tee→`wait_ready` path: a secret a child prints
    /// to stderr during startup must never reach `last_error` (which
    /// flows into `host.rs`'s log line and the UI) nor the on-disk
    /// stderr diagnostic file.
    #[test]
    fn stderr_tail_never_leaks_a_secret_to_last_error_or_disk() {
        let dir = temp_dir("redact-e2e");
        let stderr_log = dir.join("foundation.stderr.log");
        const SECRET: &str = "hunter2secretvalue";

        let mut child = if cfg!(windows) {
            let comspec = std::env::var_os("ComSpec").expect("ComSpec");
            std::process::Command::new(comspec)
                .args(["/C", &format!("echo password={SECRET} 1>&2 & exit /B 1")])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn stub")
        } else {
            std::process::Command::new("/bin/sh")
                .args(["-c", &format!("echo password={SECRET} 1>&2; exit 1")])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn stub")
        };
        let stderr = child.stderr.take().expect("stderr piped");
        let stderr_pump = crate::supervisor::spawn_redacting_tee(stderr, stderr_log.clone());

        let mut proc = SunshineProcess {
            child,
            paths: LivePaths {
                stderr_log: stderr_log.clone(),
                ..live_paths(&dir, "redact-e2e")
            },
            port: 0,
            stderr_pump,
        };

        let err = proc
            .wait_ready(Duration::from_secs(10))
            .expect_err("stub exits nonzero during startup");
        assert!(
            !err.contains(SECRET),
            "last_error must never contain the raw secret: {err}"
        );
        assert!(
            err.contains("<redacted>"),
            "redacted marker expected in last_error: {err}"
        );

        let disk = std::fs::read_to_string(&stderr_log).unwrap_or_default();
        assert!(
            !disk.contains(SECRET),
            "on-disk stderr log must never contain the raw secret: {disk}"
        );
        assert!(
            disk.contains("<redacted>"),
            "on-disk stderr log must show the redacted marker: {disk}"
        );

        proc.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
