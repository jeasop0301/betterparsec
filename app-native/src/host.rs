//! Host role — A1 slices 2+3 (design D1/D2/D3,
//! docs/design/unified-app-architecture.md §2).
//!
//! The unified app embeds the account/pairing/signaling web-server
//! in-process (`web_server::spawn_embedded`, dedicated actix thread).
//! The streamer stays a subprocess spawned by the embedded server per
//! session, resolved next to the current executable exactly like the
//! standalone binary (`sibling_streamer_candidate` uses
//! `current_exe()`, and `betterparsec.exe` ships beside `streamer.exe`).
//! Foundation Sunshine runs as a managed staged subprocess
//! (`crate::sunshine`) when `BP_SUNSHINE_STAGE` points at a stage root;
//! Sunshine startup failure degrades the host to web/pairing-only
//! instead of failing it.

use std::path::{Path, PathBuf};

use common::config::Config;

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

/// Running host role: the embedded server plus UI metadata.
pub struct Host {
    pub server: web_server::EmbeddedServer,
    pub config_source: ConfigSource,
    pub config_path: PathBuf,
    /// Managed Foundation Sunshine (None = not configured or failed).
    pub sunshine: Option<crate::sunshine::SunshineProcess>,
    /// Why Sunshine is not running (configured but failed).
    pub sunshine_error: Option<String>,
}

/// `BP_SUNSHINE_STAGE` → staged Foundation launch config, with the
/// moonlight port taken from the (shared) server config so pairing and
/// the launched host always agree. Identity defaults to
/// `<stage>/config`, overridable via `BP_SUNSHINE_IDENTITY`.
fn sunshine_config_from_env(config: &Config) -> Option<crate::sunshine::SunshineConfig> {
    let stage_root = PathBuf::from(std::env::var_os("BP_SUNSHINE_STAGE")?);
    let identity_source = std::env::var_os("BP_SUNSHINE_IDENTITY")
        .map(PathBuf::from)
        .unwrap_or_else(|| stage_root.join("config"));
    Some(crate::sunshine::SunshineConfig {
        stage_root,
        identity_source,
        port: config.moonlight.default_http_port,
    })
}

/// Start the host role from the given config path.
pub fn start(config_path: &Path) -> Result<Host, String> {
    let (config, config_source) = load_config(config_path)?;
    let sunshine_cfg = sunshine_config_from_env(&config);
    let server = web_server::spawn_embedded(config).map_err(|e| format!("{e:#}"))?;
    tracing::info!(addrs = ?server.addrs(), source = ?config_source, "host role up (embedded web-server)");

    let (sunshine, sunshine_error) = match sunshine_cfg {
        None => (None, None),
        Some(cfg) => match crate::sunshine::SunshineProcess::spawn(&cfg) {
            Ok(mut p) => match p.wait_ready(std::time::Duration::from_secs(20)) {
                Ok(()) => (Some(p), None),
                Err(e) => {
                    tracing::error!(err = %e, "sunshine startup failed — web/pairing-only host");
                    p.stop();
                    (None, Some(e))
                }
            },
            Err(e) => {
                tracing::error!(err = %e, "sunshine spawn failed — web/pairing-only host");
                (None, Some(e))
            }
        },
    };

    Ok(Host {
        server,
        config_source,
        config_path: config_path.to_path_buf(),
        sunshine,
        sunshine_error,
    })
}

impl Host {
    /// Stop the whole role: Sunshine first (sessions die with it), then
    /// the embedded server.
    pub fn stop(self) {
        if let Some(s) = self.sunshine {
            s.stop();
        }
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

        let host = start(&path).expect("host role start");
        assert_eq!(host.config_source, ConfigSource::File);
        let addr = host.server.addrs()[0];
        assert_ne!(addr.port(), 0);

        use std::io::{Read, Write};
        let mut sock = std::net::TcpStream::connect(addr).expect("connect");
        sock.write_all(b"GET /api/config.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("send");
        let mut response = Vec::new();
        sock.read_to_end(&mut response).expect("read");
        assert!(response.starts_with(b"HTTP/1.1 "), "host answered HTTP");

        host.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
