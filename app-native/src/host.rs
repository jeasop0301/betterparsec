//! Host role — A1 slices 2+3 (design D1/D2/D3,
//! docs/design/unified-app-architecture.md §2), plus G005 supervision
//! (docs unified-app-architecture.md §2 supervision addendum).
//!
//! The unified app embeds the account/pairing/signaling web-server
//! in-process (`web_server::spawn_embedded_with`, dedicated actix
//! thread). The streamer stays a subprocess spawned by the embedded
//! server per session, resolved next to the current executable exactly
//! like the standalone binary (`sibling_streamer_candidate` uses
//! `current_exe()`, and `betterparsec.exe` ships beside `streamer.exe`)
//! — now routed through the injected `web_server::StreamerLauncher` seam
//! ([`ContainedStreamerLauncher`]) so every streamer child lands in the
//! app-wide Job Object ([`crate::supervisor::assign_to_app_job`])
//! exactly like Foundation.
//!
//! Foundation Sunshine runs as a managed staged subprocess
//! (`crate::sunshine`) under a [`crate::supervisor::FoundationSupervisor`]
//! when `BP_SUNSHINE_STAGE` points at a stage root — one global
//! Foundation, budgeted restarts, a truthful [`crate::supervisor::HealthSnapshot`]
//! for the UI. Sunshine startup failure degrades the host to
//! web/pairing-only instead of failing it (`Degraded`/`Failed` state,
//! not a hard `start()` error).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::config::Config;

use crate::{identity, settings, supervisor};

/// The same default the web-server CLI uses (`src/cli.rs`), so a machine
/// that already ran the standalone server keeps its accounts/pairings.
pub const DEFAULT_CONFIG_PATH: &str = "./server/config.json";

/// Where the effective host config came from (UI display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    File,
    BuiltinDefault,
}

/// Load the host config: the standalone server's config file when
/// present, built-in defaults otherwise. Parse errors surface instead of
/// silently overriding an existing installation with defaults.
pub fn load_config(path: &Path) -> Result<(Config, ConfigSource), String> {
    match web_server::load_config_file(path) {
        Ok(Some(config)) => Ok((config, ConfigSource::File)),
        Ok(None) => Ok((Config::default(), ConfigSource::BuiltinDefault)),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Bounded graceful stop deadline for Foundation (request stop, wait
/// this long, then kill — see `supervisor::FoundationSupervisor::stop`).
const FOUNDATION_STOP_DEADLINE: Duration = Duration::from_secs(5);

/// Running host role: the embedded server, its supervised Foundation,
/// and UI metadata.
pub struct Host {
    pub server: web_server::EmbeddedServer,
    pub config_source: ConfigSource,
    pub config_path: PathBuf,
    /// Whether the embedded web-server was configured with a TLS
    /// certificate (`common::config::WebServerConfig::certificate`),
    /// captured at start time so [`pairing_hint`] can pick `http`/`https`
    /// without re-loading config.
    pub https: bool,
    /// One global Foundation, supervised (G005): Stopped when no
    /// Foundation binary is staged, otherwise driven through
    /// Starting/Healthy/Degraded/Restarting/Failed by
    /// [`supervisor::FoundationSupervisor`].
    pub foundation: Arc<supervisor::FoundationSupervisor>,
    /// Recent per-session streamer lifecycle events (G005 S1), newest
    /// last, bounded — read via [`Host::recent_streamer_events`];
    /// UI surfacing lands in G006.
    pub streamer_events: Arc<std::sync::Mutex<Vec<web_server::StreamerLifecycleEvent>>>,
    /// Stops the background foundation-monitor thread (liveness probe +
    /// `FoundationSupervisor::poll`) on [`Host::stop`].
    monitor_shutdown: Arc<AtomicBool>,
    monitor_thread: Option<std::thread::JoinHandle<()>>,
}

impl Host {
    /// Truthful Foundation health snapshot for the UI.
    pub fn foundation_health(&self) -> supervisor::HealthSnapshot {
        self.foundation.snapshot()
    }

    /// Streamer sessions launched since host start (G005 S1 lifecycle
    /// events), newest last. The host-status UI (G006) is the intended
    /// consumer; this is the read API it needs.
    pub fn recent_streamer_events(&self) -> Vec<web_server::StreamerLifecycleEvent> {
        self.streamer_events
            .lock()
            .map(|log| log.clone())
            .unwrap_or_default()
    }
}

/// G006 role UI: whether `role` should have the host role (embedded
/// server + Foundation) running. Host and Both both want it; Client
/// never does. Pure — the caller (`main.rs`'s role selector) decides
/// what to actually do with the answer.
pub fn role_wants_host(role: settings::Role) -> bool {
    matches!(role, settings::Role::Host | settings::Role::Both)
}

/// What a role change should do to the host role's running state.
/// Deliberately has no client-session parameter at all: this signature
/// has nothing *to* touch, so a caller going through this function alone
/// cannot accidentally reach a client session. That does not by itself
/// guarantee no *other* code path touches one — the actual guarantee is
/// that `main.rs`'s `apply_role_action` seam is the only place `App`
/// applies this decision, mutating `App::host` and never `App::running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleAction {
    /// Host-wanting-ness didn't change (client→client, host→both, etc).
    NoChange,
    StartHost,
    StopHost,
}

/// Pure decision: derive a [`RoleAction`] from the old/new role alone.
pub fn role_transition(old: settings::Role, new: settings::Role) -> RoleAction {
    match (role_wants_host(old), role_wants_host(new)) {
        (false, true) => RoleAction::StartHost,
        (true, false) => RoleAction::StopHost,
        _ => RoleAction::NoChange,
    }
}

/// Short UI label for a supervisor health state (G006 status panel).
pub fn health_state_label(state: supervisor::SupervisorState) -> &'static str {
    match state {
        supervisor::SupervisorState::Stopped => "stopped",
        supervisor::SupervisorState::Starting => "starting",
        supervisor::SupervisorState::Healthy => "healthy",
        supervisor::SupervisorState::Degraded => "degraded",
        supervisor::SupervisorState::Restarting => "restarting",
        supervisor::SupervisorState::Failed => "failed",
    }
}

/// One human-readable one-liner for a streamer lifecycle event (G006
/// status panel's recent-sessions list).
pub fn format_lifecycle_event(event: &web_server::StreamerLifecycleEvent) -> String {
    match event {
        web_server::StreamerLifecycleEvent::Spawned { pid } => {
            format!("streamer started (pid {pid})")
        }
        web_server::StreamerLifecycleEvent::Terminated { error_code } => {
            format!("streamer terminated (exit code {error_code})")
        }
        web_server::StreamerLifecycleEvent::Exited { pid } => {
            format!("streamer exited (pid {pid})")
        }
    }
}

/// Best-effort local LAN IPv4 address, used to turn an unspecified bind
/// address (`0.0.0.0`/`[::]` — "listen on every interface") into something
/// a user can actually type into a browser. `UdpSocket::connect` never
/// sends a packet by itself; it just asks the OS routing table which local
/// address it would use to reach the given (unreachable-in-a-sandbox-is-
/// fine) public address, which is exactly the LAN-facing address other
/// machines would dial in to. Falls back to loopback when there's no
/// route at all (offline machine, sandboxed CI, etc.) — still a valid,
/// pasteable URL, just not one another machine on the LAN can use.
fn local_lan_ipv4() -> std::net::IpAddr {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|sock| {
            sock.connect("8.8.8.8:80")?;
            sock.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

/// The embedded server's pairing/root URL for the given bound address —
/// what a user types into a browser to pair (G006 status panel hint).
/// `https` should reflect whether the web-server config that produced
/// `addr` carries a TLS certificate ([`Host::https`]); an unspecified bind
/// address is substituted with [`local_lan_ipv4`] so the hint is always a
/// concrete, pasteable address rather than the literal `0.0.0.0`.
pub fn pairing_hint(addr: std::net::SocketAddr, https: bool) -> String {
    let scheme = if https { "https" } else { "http" };
    let display_addr = if addr.ip().is_unspecified() {
        std::net::SocketAddr::new(local_lan_ipv4(), addr.port())
    } else {
        addr
    };
    format!("{scheme}://{display_addr}/")
}

/// Where the pre-G005 (unmanaged) Sunshine identity used to live —
/// exactly what this module computed before [`settings::HostSettings`]
/// existed (`BP_SUNSHINE_IDENTITY`, defaulting to `<stage>/config`).
/// [`identity::migrate`] treats a non-existent source as "nothing to
/// migrate", so an install that never used the env-var workflow yields
/// an empty (never-existing) path here and the migration is a no-op.
fn legacy_identity_source() -> PathBuf {
    match std::env::var_os("BP_SUNSHINE_STAGE").map(PathBuf::from) {
        Some(stage_root) => std::env::var_os("BP_SUNSHINE_IDENTITY")
            .map(PathBuf::from)
            .unwrap_or_else(|| stage_root.join("config")),
        None => PathBuf::new(),
    }
}

/// Legacy sources (env-var identity, standalone `DEFAULT_CONFIG_PATH`)
/// → the managed destination under `settings::HostPaths` (`dest_root` is
/// `paths.config_path`'s parent, matching `HostPaths::config_path` /
/// `HostPaths::sunshine_identity_dir` exactly per `identity.rs`'s
/// contract).
fn migration_paths(host: &settings::HostSettings) -> identity::MigrationPaths {
    let dest_root = host
        .paths
        .config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    identity::MigrationPaths {
        source_identity_dir: legacy_identity_source(),
        source_server_config: PathBuf::from(DEFAULT_CONFIG_PATH),
        dest_root,
    }
}

/// Parse `settings::generate_child_config`'s `key = value` lines into
/// `key=value` command-line overrides for the staged Foundation launch
/// (`sunshine::launch_args`'s override contract — see
/// `SunshineConfig::extra_args`), so the generated child config
/// actually drives the launched process instead of sitting unread on
/// disk (G005 dead-seam fix). `port` is deliberately skipped: it's
/// already an explicit override derived from `SunshineConfig::port`
/// (`config.moonlight.default_http_port`, not the settings-store
/// value, which can legitimately differ pre-migration), and a second,
/// possibly-conflicting `port=` argument would be ambiguous about which
/// one wins. Comment/blank lines and anything that doesn't parse as
/// `key = value` are skipped — this is a best-effort forward of
/// already-generated, already-validated settings values, not a
/// config-file parser.
fn extra_launch_args_from_child_config(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() || key == "port" {
                return None;
            }
            Some(format!("{key}={}", value.trim()))
        })
        .collect()
}

/// Resolve the staged Foundation launch config: an explicit
/// `BP_SUNSHINE_STAGE` env override (dev/test workflow) always wins;
/// otherwise the settings-managed stage, but only when a binary is
/// actually staged there — a fresh install with nothing staged yet must
/// stay `Stopped`, not `Failed`. `extra_args` carries the settings-
/// derived child-config overrides (see
/// `extra_launch_args_from_child_config`) in both branches.
fn sunshine_config_from_settings(
    config: &Config,
    settings: &settings::Settings,
) -> Option<crate::sunshine::SunshineConfig> {
    let host = &settings.host;
    let extra_args =
        extra_launch_args_from_child_config(&settings::generate_child_config(settings));
    if let Some(stage_root) = std::env::var_os("BP_SUNSHINE_STAGE").map(PathBuf::from) {
        let identity_source = std::env::var_os("BP_SUNSHINE_IDENTITY")
            .map(PathBuf::from)
            .unwrap_or_else(|| stage_root.join("config"));
        return Some(crate::sunshine::SunshineConfig {
            stage_root,
            identity_source,
            port: config.moonlight.default_http_port,
            extra_args,
        });
    }
    let stage_root = host.paths.sunshine_stage_root.clone();
    let exe = stage_root.join(format!("sunshine{}", std::env::consts::EXE_SUFFIX));
    if !exe.is_file() {
        return None;
    }
    Some(crate::sunshine::SunshineConfig {
        stage_root,
        identity_source: host.paths.sunshine_identity_dir.clone(),
        port: config.moonlight.default_http_port,
        extra_args,
    })
}

/// Adapts one Foundation launch config into a [`supervisor::ChildSpawner`]:
/// spawn, assign into the app-wide Job Object immediately (before
/// `wait_ready` — a crashed/killed unified app must never leave
/// Foundation uncontained during the up-to-20s ready-wait, LOW advisory
/// fix), then wait-ready.
struct SunshineSpawner {
    cfg: crate::sunshine::SunshineConfig,
    ready_timeout: Duration,
}

impl supervisor::ChildSpawner for SunshineSpawner {
    fn spawn(&self) -> Result<Box<dyn supervisor::SupervisedChild>, String> {
        let mut proc = crate::sunshine::SunshineProcess::spawn(&self.cfg)?;
        if let Err(e) = supervisor::assign_to_app_job(proc.raw_child().id()) {
            tracing::warn!(err = %e, "foundation job containment failed (non-fatal)");
        }
        if let Err(e) = proc.wait_ready(self.ready_timeout) {
            proc.stop();
            return Err(e);
        }
        Ok(Box::new(SunshineChildAdapter(Some(proc))))
    }
}

/// A spawner that never succeeds — used when `BP_SUNSHINE_STAGE` isn't
/// set so [`Host`] still carries a real (Stopped) supervisor rather than
/// an `Option`. `Host`/UI code never calls `start()` on it, so this path
/// is unreachable in practice.
struct UnconfiguredSpawner;

impl supervisor::ChildSpawner for UnconfiguredSpawner {
    fn spawn(&self) -> Result<Box<dyn supervisor::SupervisedChild>, String> {
        Err("BP_SUNSHINE_STAGE not set — Foundation is not configured".into())
    }
}

struct SunshineChildAdapter(Option<crate::sunshine::SunshineProcess>);

impl supervisor::SupervisedChild for SunshineChildAdapter {
    fn pid(&self) -> u32 {
        self.0.as_ref().map(|p| p.pid()).unwrap_or(0)
    }
    fn is_running(&mut self) -> bool {
        self.0.as_mut().map(|p| p.is_running()).unwrap_or(false)
    }
    fn request_stop(&mut self) {
        // No graceful shutdown IPC to Foundation in this codebase — a
        // no-op paired with `supports_graceful_stop() == false` below,
        // so `FoundationSupervisor::stop` skips its bounded-deadline
        // poll (there is nothing to cooperate with) and kills
        // immediately instead of guaranteed-stalling for the deadline.
    }
    fn supports_graceful_stop(&self) -> bool {
        false
    }
    fn kill(&mut self) {
        if let Some(p) = self.0.take() {
            p.stop();
        }
    }
    fn try_exit_code(&mut self) -> Option<i32> {
        self.0.as_mut().and_then(|p| p.try_exit_code())
    }
}

/// Streamer launches routed through the app-wide Job Object (G005 S3):
/// same spawn as [`web_server::DefaultStreamerLauncher`], plus
/// containment so a crashed/killed unified app never orphans
/// `streamer.exe`.
struct ContainedStreamerLauncher(web_server::DefaultStreamerLauncher);

#[async_trait::async_trait]
impl web_server::StreamerLauncher for ContainedStreamerLauncher {
    async fn launch(&self, streamer_path: &Path) -> Result<web_server::LaunchedStreamer, String> {
        let launched = self.0.launch(streamer_path).await?;
        if let Some(pid) = launched.child.id()
            && let Err(e) = supervisor::assign_to_app_job(pid)
        {
            tracing::warn!(err = %e, "streamer job containment failed (non-fatal)");
        }
        Ok(launched)
    }
}

/// Start the host role from the given config path, using the
/// already-loaded settings store (the caller — `App` — loads it once at
/// startup and on every settings edit; re-calling [`settings::load`] here
/// would silently re-read a possibly-stale copy from disk instead of the
/// in-memory settings the rest of the app is using).
pub fn start(config_path: &Path, settings: &settings::Settings) -> Result<Host, String> {
    let (config, config_source) = load_config(config_path)?;

    // G005: migrate any pre-managed Sunshine identity + standalone
    // server config into the managed layout before anything else reads
    // it. `recover_on_start` first — a prior migration that crashed
    // mid-run must roll back before we trust the destination. Both
    // steps are hard failures for host start: `identity.rs`'s own
    // contract is "the app must never boot on a half-migrated
    // identity" — logging and continuing here would silently violate
    // that (settings sweep finding).
    let paths = migration_paths(&settings.host);
    match identity::recover_on_start(&paths) {
        Ok(true) => tracing::warn!("recovered from an interrupted identity migration"),
        Ok(false) => {}
        Err(e) => return Err(format!("identity migration recovery failed: {e}")),
    }
    match identity::migrate(&paths) {
        Ok(report) if report.files_migrated > 0 => {
            tracing::info!(
                files_migrated = report.files_migrated,
                "migrated legacy Sunshine identity into the managed layout"
            );
        }
        Ok(_) => {}
        Err(e) => return Err(format!("legacy identity migration failed: {e}")),
    }

    // G005 S1c: regenerate the derived Sunshine/streamer child config
    // from settings on every host-role start — pure derivation, so this
    // is always safe (same settings in, byte-identical text out).
    let child_config_path = paths.dest_root.join("child.conf");
    let child_config = settings::generate_child_config(settings);
    if let Err(e) = settings::write_child_config_atomic(&child_config_path, &child_config) {
        tracing::warn!(err = %e, "failed to write generated child config (non-fatal)");
    }

    let sunshine_cfg = sunshine_config_from_settings(&config, settings);

    let streamer_events: Arc<std::sync::Mutex<Vec<web_server::StreamerLifecycleEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let events_for_sink = streamer_events.clone();
    let lifecycle: web_server::LifecycleSink = Arc::new(move |event| {
        tracing::info!(?event, "streamer lifecycle event");
        if let Ok(mut log) = events_for_sink.lock() {
            log.push(event);
            const MAX_EVENTS: usize = 256;
            if log.len() > MAX_EVENTS {
                let drop_n = log.len() - MAX_EVENTS;
                log.drain(0..drop_n);
            }
        }
    });

    // Captured before `config` moves into `spawn_embedded_with` below —
    // drives [`pairing_hint`]'s http/https choice on the UI side.
    let https = config.web_server.certificate.is_some();
    let server = web_server::spawn_embedded_with(
        config,
        Some(Arc::new(ContainedStreamerLauncher(
            web_server::DefaultStreamerLauncher,
        ))),
        Some(lifecycle),
    )
    .map_err(|e| format!("{e:#}"))?;
    tracing::info!(addrs = ?server.addrs(), source = ?config_source, "host role up (embedded web-server)");

    let foundation_events: supervisor::EventSink = Arc::new(|event| {
        tracing::info!(?event, "foundation supervisor event");
    });
    let foundation = match sunshine_cfg {
        None => Arc::new(
            supervisor::FoundationSupervisor::new(Box::new(UnconfiguredSpawner))
                .with_event_sink(foundation_events),
        ),
        Some(cfg) => {
            let sup = Arc::new(
                supervisor::FoundationSupervisor::new(Box::new(SunshineSpawner {
                    cfg,
                    ready_timeout: Duration::from_secs(20),
                }))
                .with_event_sink(foundation_events),
            );
            sup.start();
            let snapshot = sup.snapshot();
            match snapshot.state {
                supervisor::SupervisorState::Failed => {
                    tracing::error!(
                        err = ?snapshot.last_error,
                        "sunshine startup failed — web/pairing-only host"
                    );
                }
                _ => {
                    tracing::info!(pid = ?snapshot.foundation_pid, "foundation sunshine launched");
                }
            }
            sup
        }
    };

    // G005: background liveness/restart loop — drives `poll()`'s
    // restart-budget/backoff state machine on a timer instead of a
    // one-shot snapshot at boot, and keeps `note_embedded_server_alive`
    // truthful via a real TCP probe of the embedded server.
    let monitor_shutdown = Arc::new(AtomicBool::new(false));
    let monitor_thread = {
        let foundation = foundation.clone();
        let shutdown = monitor_shutdown.clone();
        let probe_addr = server.addrs().first().copied();
        std::thread::Builder::new()
            .name("bp-foundation-monitor".into())
            .spawn(move || {
                let mut last_state = None;
                while !shutdown.load(Ordering::Relaxed) {
                    let alive = probe_addr
                        .map(|addr| {
                            std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300))
                                .is_ok()
                        })
                        .unwrap_or(true);
                    foundation.note_embedded_server_alive(alive);
                    let state = foundation.poll();
                    if last_state != Some(state) {
                        tracing::info!(?state, "foundation supervisor state changed");
                        last_state = Some(state);
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .map_err(|e| tracing::warn!(err = %e, "failed to start foundation monitor thread"))
            .ok()
    };

    Ok(Host {
        server,
        config_source,
        https,
        config_path: config_path.to_path_buf(),
        foundation,
        streamer_events,
        monitor_shutdown,
        monitor_thread,
    })
}

impl Host {
    /// Stop the whole role: the foundation monitor thread first (so it
    /// stops touching `self.foundation` mid-teardown), then Sunshine
    /// (bounded graceful stop — sessions die with it), then the embedded
    /// server.
    pub fn stop(self) {
        self.monitor_shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.monitor_thread {
            let _ = t.join();
        }
        self.foundation.stop(FOUNDATION_STOP_DEADLINE);
        self.server.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bp-host-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn missing_config_file_falls_back_to_defaults() {
        let (config, source) = load_config(&temp_path("missing").join("config.json"))
            .expect("missing file is not an error");
        assert_eq!(source, ConfigSource::BuiltinDefault);
        assert_eq!(config.web_server.bind_address.port(), 8080);
    }

    #[test]
    fn broken_config_file_is_an_error_not_a_silent_default() {
        let dir = temp_path("broken");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("config.json");
        std::fs::write(&path, "{ not json !!!").expect("write");
        assert!(load_config(&path).is_err(), "parse errors must surface");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_launch_args_forward_child_config_values_except_port() {
        let content = "\
# Generated by betterparsec app-native — DO NOT EDIT
# Regenerated from settings.json on every host-role start; hand edits are lost.
port = 47990
web_port = 8080
bitrate_kbps = 6000
width = 1920
height = 1080
fps = 60
";
        let args = extra_launch_args_from_child_config(content);
        assert_eq!(
            args,
            vec![
                "web_port=8080".to_string(),
                "bitrate_kbps=6000".to_string(),
                "width=1920".to_string(),
                "height=1080".to_string(),
                "fps=60".to_string(),
            ],
            "port must be skipped (already an explicit SunshineConfig::port override); \
             everything else forwarded verbatim as key=value"
        );
    }

    /// Pins that `sunshine_config_from_settings` actually threads the
    /// settings-derived child-config overrides into the launched
    /// `SunshineConfig` (G005 dead-seam fix) rather than just generating
    /// `child.conf` and never reading it back.
    #[test]
    fn sunshine_config_carries_settings_derived_extra_args() {
        let stage = temp_path("extra-args-stage");
        std::fs::create_dir_all(&stage).expect("mkdir stage");
        std::fs::write(
            stage.join(format!("sunshine{}", std::env::consts::EXE_SUFFIX)),
            "stub",
        )
        .expect("write stub exe");

        let mut settings = settings::Settings::default();
        settings.host.paths.sunshine_stage_root = stage.clone();
        let config = Config::default();

        let cfg = sunshine_config_from_settings(&config, &settings)
            .expect("staged exe present — must resolve");
        let expected =
            extra_launch_args_from_child_config(&settings::generate_child_config(&settings));
        assert!(
            !expected.is_empty(),
            "generated child config must have forwardable keys"
        );
        assert_eq!(cfg.extra_args, expected);

        let _ = std::fs::remove_dir_all(&stage);
    }

    /// Full host-role boot: embedded server on an ephemeral port answers
    /// HTTP; stop joins the server thread.
    #[test]
    fn host_role_boots_and_stops() {
        let dir = temp_path("boot");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("config.json");
        // Serialize a real Config (no hand-written JSON to drift) and
        // prepend a comment to exercise the human-json preprocessor.
        let mut config = Config::default();
        config.data_storage = common::config::StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: std::time::Duration::from_secs(3600),
        };
        config.web_server.bind_address = "127.0.0.1:0".parse().expect("addr");
        let json = serde_json::to_string_pretty(&config).expect("serialize");
        std::fs::write(&path, format!("// human-json: comments allowed\n{json}"))
            .expect("write config");

        let host = start(&path, &settings::Settings::default()).expect("host role start");
        assert_eq!(host.config_source, ConfigSource::File);
        let addr = host.server.addrs()[0];
        assert_ne!(addr.port(), 0);

        use std::io::{Read, Write};
        let mut sock = std::net::TcpStream::connect(addr).expect("connect");
        sock.write_all(
            b"GET /api/config.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("send");
        let mut response = Vec::new();
        sock.read_to_end(&mut response).expect("read");
        assert!(response.starts_with(b"HTTP/1.1 "), "host answered HTTP");

        assert_eq!(
            host.foundation_health().state,
            supervisor::SupervisorState::Stopped,
            "no BP_SUNSHINE_STAGE in the test env — Foundation stays Stopped, not Failed"
        );

        host.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn role_wants_host_matches_host_and_both_only() {
        assert!(!role_wants_host(settings::Role::Client));
        assert!(role_wants_host(settings::Role::Host));
        assert!(role_wants_host(settings::Role::Both));
    }

    #[test]
    fn role_transition_covers_every_pair() {
        use settings::Role::{Both, Client, Host};
        // Client-not-wanting → host-wanting: start.
        assert_eq!(role_transition(Client, Host), RoleAction::StartHost);
        assert_eq!(role_transition(Client, Both), RoleAction::StartHost);
        // Host-wanting → client-not-wanting: stop.
        assert_eq!(role_transition(Host, Client), RoleAction::StopHost);
        assert_eq!(role_transition(Both, Client), RoleAction::StopHost);
        // Same wanting-ness (even across distinct roles): no change.
        assert_eq!(role_transition(Client, Client), RoleAction::NoChange);
        assert_eq!(role_transition(Host, Both), RoleAction::NoChange);
        assert_eq!(role_transition(Both, Host), RoleAction::NoChange);
        assert_eq!(role_transition(Host, Host), RoleAction::NoChange);
        assert_eq!(role_transition(Both, Both), RoleAction::NoChange);
    }

    #[test]
    fn health_state_label_covers_every_state_with_exact_text() {
        assert_eq!(
            health_state_label(supervisor::SupervisorState::Stopped),
            "stopped"
        );
        assert_eq!(
            health_state_label(supervisor::SupervisorState::Starting),
            "starting"
        );
        assert_eq!(
            health_state_label(supervisor::SupervisorState::Healthy),
            "healthy"
        );
        assert_eq!(
            health_state_label(supervisor::SupervisorState::Degraded),
            "degraded"
        );
        assert_eq!(
            health_state_label(supervisor::SupervisorState::Restarting),
            "restarting"
        );
        assert_eq!(
            health_state_label(supervisor::SupervisorState::Failed),
            "failed"
        );
    }

    #[test]
    fn lifecycle_event_formatting_mentions_the_pid_or_code() {
        assert_eq!(
            format_lifecycle_event(&web_server::StreamerLifecycleEvent::Spawned { pid: 42 }),
            "streamer started (pid 42)"
        );
        assert_eq!(
            format_lifecycle_event(&web_server::StreamerLifecycleEvent::Terminated {
                error_code: -1
            }),
            "streamer terminated (exit code -1)",
            "must assert the exact formatted fragment, not just a loose substring match"
        );
        assert_eq!(
            format_lifecycle_event(&web_server::StreamerLifecycleEvent::Exited { pid: 7 }),
            "streamer exited (pid 7)"
        );
    }

    #[test]
    fn pairing_hint_is_a_plain_http_url() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().expect("addr");
        assert_eq!(pairing_hint(addr, false), "http://127.0.0.1:8080/");
    }

    #[test]
    fn pairing_hint_uses_https_when_the_web_server_has_a_certificate() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().expect("addr");
        assert_eq!(pairing_hint(addr, true), "https://127.0.0.1:8080/");
    }

    #[test]
    fn pairing_hint_substitutes_a_concrete_address_for_an_unspecified_bind() {
        let unspecified: std::net::SocketAddr = "0.0.0.0:8080".parse().expect("addr");
        let hint = pairing_hint(unspecified, false);
        assert!(
            !hint.contains("0.0.0.0"),
            "0.0.0.0 is not something a user can type into a browser: {hint}"
        );
        assert!(
            hint.starts_with("http://"),
            "no certificate => http: {hint}"
        );
        assert!(hint.ends_with(":8080/"), "port must be preserved: {hint}");

        let unspecified_v6: std::net::SocketAddr = "[::]:8080".parse().expect("addr");
        let hint_v6 = pairing_hint(unspecified_v6, true);
        assert!(
            !hint_v6.contains("[::]"),
            "[::] must be substituted: {hint_v6}"
        );
        assert!(
            hint_v6.starts_with("https://"),
            "certificate => https: {hint_v6}"
        );
    }
}
