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
use std::time::{Duration, Instant};

/// How to launch the staged Foundation Sunshine.
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
fn launch_args(paths: &LivePaths, port: u16) -> Vec<String> {
    vec![
        paths.config_file.to_string_lossy().into_owned(),
        format!("port={port}"),
        "upnp=disabled".into(),
        format!("pkey={}", paths.private_key.to_string_lossy()),
        format!("cert={}", paths.certificate.to_string_lossy()),
        format!("file_state={}", paths.state_file.to_string_lossy()),
        format!("credentials_file={}", paths.state_file.to_string_lossy()),
        format!("file_apps={}", paths.apps_file.to_string_lossy()),
        format!("log_path={}", paths.sunshine_log.to_string_lossy()),
    ]
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

        let stdout = std::fs::File::create(&paths.stdout_log)
            .map_err(|e| format!("create stdout log: {e}"))?;
        let stderr = std::fs::File::create(&paths.stderr_log)
            .map_err(|e| format!("create stderr log: {e}"))?;
        let child = std::process::Command::new(&exe)
            .args(launch_args(&paths, cfg.port))
            .current_dir(&cfg.stage_root)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("launch {}: {e}", exe.display()))?;
        tracing::info!(pid = child.id(), port = cfg.port, live = %paths.root.display(),
            "foundation sunshine launched");
        Ok(Self {
            child,
            paths,
            port: cfg.port,
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
                return Err(format!(
                    "sunshine exited during startup ({status}); stderr: {}",
                    tail_of(&self.paths.stderr_log, 512)
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
        let args = launch_args(&paths, 49_000);
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
    fn spawn_copies_identity_into_a_fresh_live_dir() {
        let stage = stub_stage("live");
        let identity = temp_dir("identity");
        identity_fixture(&identity);

        let proc = SunshineProcess::spawn(&SunshineConfig {
            stage_root: stage.clone(),
            identity_source: identity.clone(),
            port: 0, // bind pre-check: port 0 always bindable
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
        })
        .expect_err("port busy");
        assert!(err.contains("already in use"), "{err}");
        drop(listener);
        let _ = std::fs::remove_dir_all(&empty);
        let _ = std::fs::remove_dir_all(&stage);
        let _ = std::fs::remove_dir_all(&identity);
    }
}
