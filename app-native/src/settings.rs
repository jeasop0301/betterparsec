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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

impl Default for ClientSettings {
    fn default() -> Self {
        // Override fields default to 0 = "use the selected mode's default"
        // (`knobs_for` treats a 0 field as no-override). Seeding the
        // concrete Medium numbers here would pin them regardless of the
        // chosen mode — the mode selector must actually change the
        // resolved bitrate/resolution, so first run stays 0 and resolves
        // to Medium's knobs via `to_flowconfig_fields`.
        ClientSettings {
            mode: StreamMode::default(),
            bitrate_kbps: 0,
            width: 0,
            height: 0,
            fps: 0,
            present_10bit: false,
            client_cursor: false,
        }
    }
}

/// The on-disk settings document. `unknown` preserves any top-level keys
/// this build doesn't recognize (forward/back-compat: an older or newer
/// build's `save()` never drops fields it doesn't understand).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema: u32,
    pub client: ClientSettings,
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            schema: 1,
            client: ClientSettings::default(),
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
/// keeps every field neither one recognizes.
pub fn save_to(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
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
                .unwrap()
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
        let dir = path.parent().unwrap();
        let stem = path.file_name().unwrap().to_string_lossy().to_string();
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
    fn parse_error_surfaced() {
        // Corrupt-handling variant: the parse error itself is not
        // swallowed silently — it is logged (tracing::warn in
        // load_from) and the caller gets back known-good defaults
        // rather than a partially-deserialized/garbage Settings.
        let path = tmp_path("parse-error-surfaced");
        std::fs::write(&path, "not json").expect("write junk");
        let result = load_from(&path);
        assert_eq!(result, Ok(Settings::default()));
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
}
