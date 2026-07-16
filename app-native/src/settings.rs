//! User settings store — G005 S1b (config unification).
//!
//! Precedence lowest→highest across the whole app: builtin defaults <
//! deploy-seed `betterparsec.conf` (see `main.rs::packaged_conf`) < THIS
//! store (`%APPDATA%/betterparsec/settings.json`) < per-connection <
//! dev env (`BP_*`, highest — see `ConnectForm::default` and
//! `present_10bit_from_env`). This module owns only the user-store slot:
//! it never reads `betterparsec.conf` or `BP_*` itself, callers fold
//! precedence on top.
//!
//! Free-lunch levers stay code consts (`transport_core::mode::free_lunch`,
//! private) — the user only ever picks the 3-way [`transport_core::mode::StreamMode`]
//! plus bitrate/resolution/fps overrides, mirroring the mode engine's
//! philosophy (mode.rs module doc).
//!
//! Human-json parsing reuses `web_server::human_json::preprocess_human_json`
//! (A5 finding: `pub mod human_json` is exported from `src/lib.rs`, and
//! `preprocess_human_json` is `pub fn` — reachable from app-native exactly
//! like `host.rs::load_config` reuses `web_server::load_config_file`, which
//! preprocesses the same way internally).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use transport_core::mode::{StreamMode, UserTradeoffs, knobs_for};

/// Custom (de)serialization for the foreign [`StreamMode`] type (orphan
/// rule forbids `impl Serialize for StreamMode` here) — stored on disk as
/// a lowercase string (`"fast"` / `"medium"` / `"quality"`); anything
/// unrecognized on read falls back to [`StreamMode::default`] (`Medium`)
/// rather than failing the whole file.
mod mode_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use transport_core::mode::StreamMode;

    pub fn serialize<S: Serializer>(mode: &StreamMode, s: S) -> Result<S::Ok, S::Error> {
        match mode {
            StreamMode::Fast => "fast",
            StreamMode::Medium => "medium",
            StreamMode::Quality => "quality",
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<StreamMode, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.trim().to_ascii_lowercase().as_str() {
            "fast" => StreamMode::Fast,
            "quality" => StreamMode::Quality,
            _ => StreamMode::Medium,
        })
    }
}

/// The user-picked client tradeoffs: 3-way mode plus the four genuine
/// per-field overrides (`transport_core::mode::UserTradeoffs`), plus the
/// one opt-in free lever with no `ModeKnobs` slot (`present_10bit` folds
/// into `Renderer::new_inner`'s `want_10bit`, see `present.rs`).
///
/// Derived `Default` = mode Medium + every override field 0. A 0 override
/// means "use the selected mode's default" (`knobs_for` treats 0 as
/// no-override), so first run resolves to Medium and switching mode
/// actually changes the resolved bitrate/resolution — seeding concrete
/// numbers would pin one mode regardless of the selector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ClientSettings {
    #[serde(with = "mode_serde")]
    pub mode: StreamMode,
    pub bitrate_kbps: u32,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub present_10bit: bool,
    /// Client-side rendered host cursor (M4 cursor P2, cursor_icon.rs).
    /// `BP_CLIENT_CURSOR=1` still overrides this (dev-only, highest slot).
    pub client_cursor: bool,
}

/// Which role(s) this install runs. Purely a settings-schema switch (G005
/// S1c) — it does not itself start/stop the host role; consuming `role`
/// to auto-boot `host::start` alongside the client shell is the G006
/// role-UI wiring: `App::new` and the host-section role-select combo box
/// (see `main.rs::App::host_section`) spawn `host::start` on a background
/// thread (`App::start_host`) whenever `role` wants the host role, and
/// stop it when it stops wanting it. Backward-compatible: an old store predating this field
/// deserializes with `#[serde(default)]` on [`Settings`] and gets
/// `Role::Client` (today's only behavior), never a hard parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Client,
    Host,
    Both,
}

/// Where the managed host role keeps everything it owns on disk, all
/// under `%APPDATA%/betterparsec/` so a fresh install never scatters
/// state next to the exe. `host/` holds the server account/pairing
/// config plus the migrated Sunshine identity and the generated child
/// config (see [`generate_child_config`]); `logs/` and `updates/` are
/// separate top-level siblings (consolidated log capture, G006 update
/// staging) rather than nested under `host/`, so a client-only install
/// that later adds host role doesn't need to move anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostPaths {
    /// Server account/pairing config (`web_server::Config`, `host.rs`'s
    /// managed replacement for the standalone `./server/config.json`).
    pub config_path: PathBuf,
    /// Staged Foundation Sunshine binary root (`sunshine.rs::SunshineConfig::stage_root`).
    pub sunshine_stage_root: PathBuf,
    /// Migrated Sunshine identity (certs/state/apps) — the managed
    /// `identity_source` `sunshine.rs::SunshineConfig` launches from,
    /// and `identity.rs`'s migration destination.
    pub sunshine_identity_dir: PathBuf,
    /// Consolidated host/child-process logs.
    pub logs_dir: PathBuf,
    /// G006 update staging (download/verify area; no update logic here).
    pub updates_dir: PathBuf,
}

impl Default for HostPaths {
    fn default() -> Self {
        let base = data_dir();
        HostPaths {
            config_path: base.join("host").join("config.json"),
            sunshine_stage_root: base.join("host").join("sunshine"),
            sunshine_identity_dir: base.join("host").join("identity"),
            logs_dir: base.join("logs"),
            updates_dir: base.join("updates"),
        }
    }
}

/// Host-role network binding — must agree with whatever Sunshine/the
/// embedded server actually bind so pairing and the generated child
/// config stay in lockstep (see [`generate_child_config`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostNetwork {
    /// Embedded account/pairing/signaling web-server port
    /// (`common::config::WebServerConfig::bind_address` default: 8080).
    pub web_port: u16,
    /// Moonlight HTTP port Sunshine/pairing use
    /// (`common::config::MoonlightConfig::default_http_port` default:
    /// 47989).
    pub moonlight_port: u16,
}

impl Default for HostNetwork {
    fn default() -> Self {
        HostNetwork {
            web_port: 8080,
            moonlight_port: 47989,
        }
    }
}

/// Update channel placeholders only — real update-check/apply logic is
/// G006; this just reserves the schema slot so a future build doesn't
/// need another migration to add it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HostUpdate {
    pub channel: String,
    pub url: String,
}

/// The full host-role settings section. Read (not written) by the
/// managed-host supervision lane (`host.rs`/`sunshine.rs`) — see the
/// `HostSettings`/`HostPaths` field-name contract agreed with that lane
/// over IRC (G005 split): `paths.sunshine_stage_root`,
/// `paths.sunshine_identity_dir`, `paths.logs_dir` are the fields it
/// consumes; `network`/`update` are settings-only for now. `unknown`
/// preserves any host-section keys this build doesn't recognize, the
/// same forward/back-compat guarantee [`Settings::unknown`] gives
/// top-level keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HostSettings {
    pub paths: HostPaths,
    pub network: HostNetwork,
    pub update: HostUpdate,
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// The on-disk settings document. `unknown` preserves any top-level keys
/// this build doesn't recognize (forward/back-compat: an older or newer
/// build's `save()` never drops fields it doesn't understand).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema: u32,
    pub client: ClientSettings,
    /// client|host|both — see [`Role`].
    pub role: Role,
    /// Host-role paths/network/update — see [`HostSettings`].
    pub host: HostSettings,
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            schema: 1,
            client: ClientSettings::default(),
            role: Role::default(),
            host: HostSettings::default(),
            unknown: serde_json::Map::new(),
        }
    }
}

/// `bitrate_kbps, width, height, fps, supported_codecs` — the fields
/// `client-transport::flow::FlowConfig` needs, derived through the pure
/// mode engine so a store edit and a fresh install agree bit-for-bit with
/// `knobs_for`. Codec: `transport_core::mode::CodecPref` maps onto
/// `client_transport::session`'s `*_BIT` constants in a later wiring goal
/// (mode.rs module doc); only `H264_BIT` exists today, so every
/// `CodecPref` currently collapses onto it — documented here rather than
/// silently dropping the preference.
pub fn to_flowconfig_fields(client: &ClientSettings) -> (u32, u32, u32, u32, u32) {
    let knobs = knobs_for(
        client.mode,
        UserTradeoffs {
            bitrate_kbps: client.bitrate_kbps,
            width: client.width,
            height: client.height,
            fps: client.fps,
        },
    );
    let supported_codecs = client_transport::session::H264_BIT;
    (
        knobs.bitrate_kbps,
        knobs.width as u32,
        knobs.height as u32,
        knobs.fps as u32,
        supported_codecs,
    )
}

/// The base `%APPDATA%/betterparsec` (or XDG-equivalent) data directory,
/// derived from [`default_settings_path`]'s parent. `HostPaths::default`
/// nests `host/`, `logs/`, `updates/` under this same root so client-only
/// and host-role installs share one data directory. Falls back to `.`
/// (relative to cwd) in a headless/CI env with no usable base dir — the
/// same "never touch disk unexpectedly, just resolve *some* path" stance
/// as the rest of this module; callers still gate actual disk I/O behind
/// their own checks.
fn data_dir() -> PathBuf {
    default_settings_path()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `%APPDATA%/betterparsec/settings.json` on Windows; `$XDG_CONFIG_HOME`
/// or `$HOME/.config/betterparsec/settings.json` elsewhere. `None` when no
/// usable base directory is set (headless/CI env) — callers fall back to
/// in-memory defaults without touching disk.
pub fn default_settings_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|appdata| {
            PathBuf::from(appdata)
                .join("betterparsec")
                .join("settings.json")
        })
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return Some(
                PathBuf::from(xdg)
                    .join("betterparsec")
                    .join("settings.json"),
            );
        }
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("betterparsec")
                .join("settings.json")
        })
    }
}

/// Load settings from the default location. Never fails: a missing file,
/// an unreadable base directory, or a genuine I/O error all fall back to
/// in-memory defaults (logged via `tracing::warn`) — the user store is
/// the lowest-friction slot in the precedence chain, so the app must
/// always come up.
pub fn load() -> Settings {
    match default_settings_path() {
        Some(path) => load_from(&path).unwrap_or_else(|e| {
            tracing::warn!(err = %e, "settings store load failed — using defaults");
            Settings::default()
        }),
        None => Settings::default(),
    }
}

/// Core load: missing file → `Ok(defaults)` (not an error — first run).
/// Corrupt-but-present file → back it up to `settings.json.bak-<unix_ts>`,
/// regenerate defaults on disk, and still return `Ok(defaults)` (the
/// store is user-owned; unlike `host.rs::load_config`'s hard error on a
/// corrupt *deployment* config, corrupt *user* settings shouldn't block
/// the app — but they must never be silently discarded without a backup).
/// Genuine I/O errors (unreadable file present, backup/regen write
/// failure) surface as `Err` for the caller to log.
pub fn load_from(path: &Path) -> Result<Settings, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let processed = web_server::human_json::preprocess_human_json(raw);
    match serde_json::from_str::<Settings>(&processed) {
        Ok(settings) => Ok(settings),
        Err(parse_err) => {
            tracing::warn!(
                err = %parse_err,
                path = %path.display(),
                "corrupt settings store — backing up and regenerating defaults"
            );
            backup_corrupt(path)
                .map_err(|e| format!("corrupt settings ({parse_err}) and backup failed: {e}"))?;
            let defaults = Settings::default();
            save_to(path, &defaults)
                .map_err(|e| format!("corrupt settings backed up but regeneration failed: {e}"))?;
            Ok(defaults)
        }
    }
}

/// Back up a corrupt (but present) store file to `<name>.bak-<unix_ts>`
/// next to it, so a populated file is never silently defaulted-over.
fn backup_corrupt(path: &Path) -> Result<(), String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut backup = path.as_os_str().to_owned();
    backup.push(format!(".bak-{ts}"));
    std::fs::copy(path, PathBuf::from(backup)).map_err(|e| e.to_string())?;
    Ok(())
}

/// Save to the default location. `None` default path (no usable base
/// dir) is a no-op success — nothing to persist to, and the in-memory
/// settings still govern this run.
pub fn save(settings: &Settings) -> Result<(), String> {
    match default_settings_path() {
        Some(path) => save_to(&path, settings),
        None => Ok(()),
    }
}

/// Core save: pretty JSON, preserving `unknown` (via `#[serde(flatten)]`
/// on `Settings`) — a round trip through an older/newer build's `save()`
/// keeps every field neither one recognizes. Atomic like
/// [`write_child_config_atomic`]: a torn write must never corrupt the
/// user's store (the corrupt-backup-regenerate path is for external
/// corruption, not our own saves).
pub fn save_to(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} -> {}: {e}", tmp.display(), path.display())
    })
}

/// Pure derivation of the generated runtime Sunshine/streamer child
/// config content from settings (G005 S1c "atomic child-config
/// generation"). Users never hand-edit the file this produces — it is
/// fully re-derivable from `Settings` alone, so regenerating it (e.g. on
/// every host-role start) is always safe and idempotent: same settings
/// in, byte-identical text out, every time. Reuses
/// [`to_flowconfig_fields`] so the generated child config and a live
/// client session agree on bitrate/resolution/fps bit-for-bit with the
/// mode engine, exactly like the client store does.
pub fn generate_child_config(settings: &Settings) -> String {
    let (bitrate_kbps, width, height, fps, _supported_codecs) =
        to_flowconfig_fields(&settings.client);
    let mut lines = vec![
        "# Generated by betterparsec app-native — DO NOT EDIT".to_string(),
        "# Regenerated from settings.json on every host-role start; hand edits are lost."
            .to_string(),
        format!("port = {}", settings.host.network.moonlight_port),
        format!("web_port = {}", settings.host.network.web_port),
        format!("bitrate_kbps = {bitrate_kbps}"),
        format!("width = {width}"),
        format!("height = {height}"),
        format!("fps = {fps}"),
    ];
    lines.push(String::new());
    lines.join("\n")
}

/// Write generated content to `path` atomically: write to a sibling temp
/// file, then `rename` over the destination. On the same volume (always
/// true here — the temp file lives next to `path`) a rename is atomic, so
/// a reader never observes a partially-written child config, and a crash
/// mid-write leaves the previous good file untouched (the temp file is
/// simply orphaned, not the live config). The temp file is removed on a
/// failed rename so a failed attempt never leaks a stray sibling.
pub fn write_child_config_atomic(path: &Path, content: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{}: config path has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;

    let tmp_name = format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("child-config"),
        std::process::id()
    );
    let tmp_path = parent.join(tmp_name);
    std::fs::write(&tmp_path, content)
        .map_err(|e| format!("write temp {}: {e}", tmp_path.display()))?;

    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(format!(
                "rename {} -> {}: {e}",
                tmp_path.display(),
                path.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "bp-settings-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the unix epoch")
                .as_nanos()
        ));
        p
    }

    #[test]
    fn load_missing_returns_defaults() {
        let path = tmp_path("missing");
        assert!(!path.exists());
        let settings = load_from(&path).expect("missing file is not an error");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn unknown_fields_preserved_roundtrip() {
        let path = tmp_path("unknown");
        let mut settings = Settings::default();
        settings
            .unknown
            .insert("future_field".into(), serde_json::json!({"x": 1}));
        save_to(&path, &settings).expect("save");

        let loaded = load_from(&path).expect("load");
        assert_eq!(
            loaded.unknown.get("future_field"),
            Some(&serde_json::json!({"x": 1}))
        );
        assert_eq!(loaded.schema, settings.schema);
        assert_eq!(loaded.client, settings.client);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_backs_up_and_regenerates() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "{ not valid json at all ][").expect("write junk");

        let loaded = load_from(&path).expect("corrupt is backed up, not a hard error");
        assert_eq!(loaded, Settings::default());

        // A backup sibling with the .bak-<ts> suffix must exist.
        let dir = path.parent().expect("tmp path has a parent dir");
        let stem = path
            .file_name()
            .expect("tmp path has a file name")
            .to_string_lossy()
            .to_string();
        let backed_up = std::fs::read_dir(dir)
            .expect("read tmp dir")
            .filter_map(|e| e.ok())
            .any(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with(&format!("{stem}.bak-"))
            });
        assert!(backed_up, "expected a {stem}.bak-<ts> backup sibling");

        // Regenerated file on disk parses back to defaults too.
        let regenerated = load_from(&path).expect("regenerated file loads");
        assert_eq!(regenerated, Settings::default());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_error_backs_up_the_corrupt_file_before_regenerating() {
        // Corrupt-handling variant distinct from
        // `corrupt_backs_up_and_regenerates` (which asserts the .bak
        // sibling appears): this pins that the *returned* value is exactly
        // the known-good defaults — never a partially-deserialized or
        // garbage Settings — and that the corrupt bytes survive in the
        // backup rather than being silently destroyed.
        let path = tmp_path("parse-error-surfaced");
        std::fs::write(&path, "not json").expect("write junk");
        let result = load_from(&path);
        assert_eq!(result, Ok(Settings::default()));

        let dir = path.parent().expect("tmp path has a parent dir");
        let stem = path
            .file_name()
            .expect("tmp path has a file name")
            .to_string_lossy()
            .to_string();
        let backup = std::fs::read_dir(dir)
            .expect("read tmp dir")
            .filter_map(|e| e.ok())
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{stem}.bak-"))
            })
            .expect("corrupt file is backed up, not destroyed");
        let preserved = std::fs::read_to_string(backup.path()).expect("read backup");
        assert_eq!(
            preserved, "not json",
            "backup keeps the corrupt bytes verbatim"
        );

        let _ = std::fs::remove_file(backup.path());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn flowconfig_derivation_from_store() {
        let client = ClientSettings {
            mode: StreamMode::Quality,
            bitrate_kbps: 0,
            width: 0,
            height: 0,
            fps: 0,
            present_10bit: false,
            client_cursor: false,
        };
        let (bitrate, width, height, fps, codecs) = to_flowconfig_fields(&client);
        let knobs = knobs_for(StreamMode::Quality, UserTradeoffs::default());
        assert_eq!(bitrate, knobs.bitrate_kbps);
        assert_eq!(width, knobs.width as u32);
        assert_eq!(height, knobs.height as u32);
        assert_eq!(fps, knobs.fps as u32);
        assert_eq!(codecs, client_transport::session::H264_BIT);
    }

    #[test]
    fn flowconfig_derivation_honors_user_override() {
        let client = ClientSettings {
            mode: StreamMode::Fast,
            bitrate_kbps: 12_345,
            width: 0,
            height: 0,
            fps: 0,
            present_10bit: true,
            client_cursor: false,
        };
        let (bitrate, ..) = to_flowconfig_fields(&client);
        assert_eq!(bitrate, 12_345);
    }

    #[test]
    fn default_store_mode_switch_changes_flowconfig() {
        // P1 guard: with a DEFAULT store (0-sentinel overrides), switching
        // mode must actually change the resolved bitrate/resolution — the
        // default must NOT pre-seed concrete numbers that pin one mode.
        let mut c = ClientSettings::default();
        assert_eq!(
            (c.bitrate_kbps, c.width, c.height, c.fps),
            (0, 0, 0, 0),
            "default overrides are 0-sentinels, not concrete numbers"
        );
        c.mode = StreamMode::Fast;
        let fast = to_flowconfig_fields(&c);
        c.mode = StreamMode::Quality;
        let quality = to_flowconfig_fields(&c);
        assert_ne!(
            fast, quality,
            "mode selector must change the resolved fields"
        );
        let fk = knobs_for(StreamMode::Fast, UserTradeoffs::default());
        assert_eq!(fast.0, fk.bitrate_kbps);
        assert_eq!(fast.1, fk.width as u32);
    }

    #[test]
    fn mode_serde_roundtrips_and_defaults_unknown() {
        let json = serde_json::json!({"schema": 1, "client": {"mode": "quality"}});
        let settings: Settings = serde_json::from_value(json).expect("parse");
        assert_eq!(settings.client.mode, StreamMode::Quality);

        let json_bad = serde_json::json!({"schema": 1, "client": {"mode": "bogus"}});
        let settings_bad: Settings = serde_json::from_value(json_bad).expect("parse");
        assert_eq!(settings_bad.client.mode, StreamMode::default());
    }

    #[test]
    fn client_cursor_defaults_off_and_persists() {
        // S1d: default off; the store value round-trips (env override is
        // folded on top in main.rs, not here — this is the store slot only).
        assert!(!ClientSettings::default().client_cursor);
        let path = tmp_path("cursor");
        let mut s = Settings::default();
        s.client.client_cursor = true;
        save_to(&path, &s).expect("save");
        let loaded = load_from(&path).expect("load");
        assert!(
            loaded.client.client_cursor,
            "store value persists round-trip"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn role_and_host_schema_roundtrip() {
        // G005 S1c: role + host section round-trip through the store,
        // and defaults come back byte-for-byte equal to `Settings::default`.
        let path = tmp_path("role-host-roundtrip");
        let mut settings = Settings {
            role: Role::Both,
            ..Default::default()
        };
        settings.host.paths.sunshine_stage_root = PathBuf::from("C:/stage");
        settings.host.paths.sunshine_identity_dir = PathBuf::from("C:/identity");
        settings.host.paths.logs_dir = PathBuf::from("C:/logs");
        settings.host.paths.updates_dir = PathBuf::from("C:/updates");
        settings.host.paths.config_path = PathBuf::from("C:/host/config.json");
        settings.host.network.web_port = 9090;
        settings.host.network.moonlight_port = 47000;
        settings.host.update.channel = "beta".into();
        settings.host.update.url = "https://example.invalid/update".into();
        save_to(&path, &settings).expect("save");

        let loaded = load_from(&path).expect("load");
        assert_eq!(loaded, settings, "role/host section round-trips exactly");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_keys_nested_under_host_are_preserved() {
        // Forward-compat inside the host section: a newer build's
        // `host.some_future_host_key` must survive this build's
        // load -> save round trip, exactly like unknown top-level keys.
        let path = tmp_path("host-unknown");
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": 1,
                "role": "host",
                "host": {
                    "network": {"web_port": 9191, "moonlight_port": 47989},
                    "some_future_host_key": {"x": 1}
                }
            })
            .to_string(),
        )
        .expect("write store with nested host unknown");

        let loaded = load_from(&path).expect("load");
        assert_eq!(loaded.host.network.web_port, 9191);
        assert_eq!(
            loaded.host.unknown.get("some_future_host_key"),
            Some(&serde_json::json!({"x": 1}))
        );

        save_to(&path, &loaded).expect("save");
        let reloaded = load_from(&path).expect("reload");
        assert_eq!(
            reloaded.host.unknown.get("some_future_host_key"),
            Some(&serde_json::json!({"x": 1})),
            "nested host unknown key survives a full save round trip"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_store_missing_role_and_host_defaults_client() {
        // Backward compatibility: a store predating G005 has no
        // "role"/"host" keys at all — it must still parse (not a hard
        // failure) and fall back to Role::Client + HostSettings::default,
        // while any of *its* unknown top-level keys are still preserved.
        let path = tmp_path("legacy-no-role-host");
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": 1,
                "client": {"mode": "fast"},
                "some_future_top_level_key": 42
            })
            .to_string(),
        )
        .expect("write legacy store");

        let loaded = load_from(&path).expect("legacy store parses");
        assert_eq!(loaded.role, Role::Client);
        assert_eq!(loaded.host, HostSettings::default());
        assert_eq!(loaded.client.mode, StreamMode::Fast);
        assert_eq!(
            loaded.unknown.get("some_future_top_level_key"),
            Some(&serde_json::json!(42))
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn role_serializes_as_lowercase_strings() {
        let json = serde_json::to_value(Role::Host).expect("serialize");
        assert_eq!(json, serde_json::json!("host"));
        let json = serde_json::to_value(Role::Both).expect("serialize");
        assert_eq!(json, serde_json::json!("both"));
        let parsed: Role = serde_json::from_value(serde_json::json!("both")).expect("parse");
        assert_eq!(parsed, Role::Both);
    }

    #[test]
    fn generated_child_config_is_deterministic() {
        // G005 S1c: same settings in, byte-identical text out, every
        // time — no timestamps/randomness allowed in generated content.
        let mut settings = Settings::default();
        settings.host.network.moonlight_port = 47123;
        settings.host.network.web_port = 8123;
        settings.client.mode = StreamMode::Quality;

        let a = generate_child_config(&settings);
        let b = generate_child_config(&settings);
        assert_eq!(a, b);
        assert!(a.contains("port = 47123"));
        assert!(a.contains("web_port = 8123"));

        let knobs = knobs_for(StreamMode::Quality, UserTradeoffs::default());
        assert!(a.contains(&format!("bitrate_kbps = {}", knobs.bitrate_kbps)));
    }

    #[test]
    fn write_child_config_atomic_writes_content_and_leaves_no_temp_file() {
        let dir = tmp_path("child-config-dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("sunshine.conf");

        let settings = Settings::default();
        let content = generate_child_config(&settings);
        write_child_config_atomic(&target, &content).expect("write atomic");

        let on_disk = std::fs::read_to_string(&target).expect("read written config");
        assert_eq!(on_disk, content);

        // No stray temp sibling left behind after a successful rename.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file must be cleaned up");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_child_config_atomic_regeneration_is_idempotent() {
        let dir = tmp_path("child-config-regen");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("sunshine.conf");

        let settings = Settings::default();
        let content = generate_child_config(&settings);
        write_child_config_atomic(&target, &content).expect("first write");
        write_child_config_atomic(&target, &content).expect("second write (regeneration)");

        let on_disk = std::fs::read_to_string(&target).expect("read written config");
        assert_eq!(on_disk, content, "regeneration is idempotent");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
