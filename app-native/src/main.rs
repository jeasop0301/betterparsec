//! BetterParsec unified app — A0 client first light
//! (docs/design/unified-app-architecture.md §6).
//!
//! Slices 1–3: native shell (egui chrome) + client-transport session +
//! FFmpeg H.264 decode (`video` feature: D3D11VA hwaccel, software
//! fallback) + raw D3D11 FLIP_DISCARD present on a dedicated stream
//! child HWND (design D5/D9; interim egui texture as fallback and on
//! non-Windows). Built without `video`, received frames are counted and
//! dropped, which still runs the whole session pipeline (signaling →
//! WebRTC → video_fec → FEC decode → FrameQueue) for real.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(all(windows, feature = "video"))]
mod audio;
#[cfg(all(windows, feature = "video"))]
mod cursor_icon;
mod host;
#[cfg(all(windows, feature = "video"))]
mod immersive;
#[cfg(all(windows, feature = "video"))]
mod input;
#[cfg(all(windows, feature = "video"))]
mod present;
mod settings;
mod sunshine;
#[cfg(feature = "video")]
mod video;

#[cfg(feature = "video")]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use client_transport::capi::RxCore;
use client_transport::flow::FlowConfig;
use client_transport::session::{Session, SessionConfig, SessionState};
use client_transport::tls::ServerTrust;
#[cfg(feature = "video")]
use transport_core::video_rx::DecodeUnit;

fn main() -> eframe::Result {
    // Console + always-on file log (betterparsec.log next to the exe,
    // append). The field client is double-clicked — no env, no console
    // capture — so without the file sink a mid-session stall leaves zero
    // evidence. `betterparsec::input=debug` is on by default for the
    // cursor/immersive diagnostics; RUST_LOG still overrides everything.
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            "info,betterparsec::input=debug,webrtc=warn,webrtc_ice=warn,webrtc_sctp=warn".into()
        });
        let file = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("betterparsec.log")))
            .and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
            });
        let registry = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer());
        match file {
            Some(f) => registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Mutex::new(f)),
                )
                .init(),
            None => registry.init(),
        }
    }
    tracing::info!("=== betterparsec build 07-16l starting ===");

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_title("BetterParsec — build 07-16l (remote Alt + lossless FEC ingress)"),
        ..Default::default()
    };
    eframe::run_native(
        "BetterParsec",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

// ── Live receive stats (pump thread → UI) ─────────────────────────────────

#[derive(Default)]
struct RxStats {
    frames: AtomicU64,
    key_frames: AtomicU64,
    payload_bytes: AtomicU64,
    /// ms since pump start of the most recent frame (0 = none yet).
    last_frame_ms: AtomicU64,
    stopped: AtomicBool,
}

/// Window of recent frame times for an fps readout.
#[derive(Default)]
struct FpsWindow(Mutex<Vec<Instant>>);

impl FpsWindow {
    fn push(&self, now: Instant) {
        let mut v = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        v.push(now);
        let cutoff = now - Duration::from_secs(2);
        v.retain(|t| *t >= cutoff);
    }

    fn fps(&self) -> f64 {
        let v = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        v.len() as f64 / 2.0
    }
}

/// Decode output shared pump → UI (`video` feature).
#[cfg(feature = "video")]
#[derive(Default)]
struct VideoShared {
    /// Newest decoded picture; the presenter takes it (newest-wins).
    frame: Mutex<Option<video::DecodedFrame>>,
    /// Signaled per stored frame; the raw present thread blocks here.
    frame_ready: Condvar,
    /// Bumped once per stored frame so the UI knows when to re-upload
    /// (egui fallback path).
    generation: AtomicU64,
    /// Latest decoded dimensions, packed `w << 32 | h` (aspect fit).
    dims: AtomicU64,
    /// Raw D3D11 surface init/present gave up — use the egui fallback.
    raw_present_failed: AtomicBool,
    /// Tells the present thread to exit (surface teardown).
    present_stop: AtomicBool,
    decoded: AtomicU64,
    decode_errors: AtomicU64,
    hw_device: AtomicBool,
    /// Client-side sharpen strength, integer percent 0..100 (0 = off).
    /// Live-adjustable from the shell UI; the present thread reads it each
    /// frame. Seeded from `BP_SHARPEN` at surface creation.
    sharpen_pct: std::sync::atomic::AtomicU32,
    /// Opt-in GPU NV12 present: the decoder emits NV12 planes and the raw
    /// present converts on the GPU (skips the CPU swscale + RGBA upload).
    /// Live toggle from the shell UI; only takes effect while the raw D3D11
    /// present and hardware decode are both active. Default off (RGBA).
    nv12: AtomicBool,
}

#[cfg(feature = "video")]
impl VideoShared {
    /// The raw D3D11 present path is (still) responsible for drawing.
    fn raw_present_active(&self) -> bool {
        cfg!(windows) && !self.raw_present_failed.load(Ordering::Acquire)
    }
}

/// Per-pump decoder state: FFmpeg decoder + IDR gating.
#[cfg(feature = "video")]
struct DecodeState {
    decoder: Option<video::Decoder>,
    /// Skip deltas until the first IDR (re-armed after a decode error) so
    /// the decoder never chews frames whose references it cannot have.
    wait_for_key: bool,
}

#[cfg(feature = "video")]
impl DecodeState {
    fn new(shared: &VideoShared) -> Self {
        let decoder = match video::Decoder::new() {
            Ok(d) => {
                shared.hw_device.store(d.hw_device, Ordering::Relaxed);
                tracing::info!(hw = d.hw_device, "video decoder ready");
                Some(d)
            }
            Err(e) => {
                tracing::error!(err = %e, "video decoder init failed — receive-only mode");
                None
            }
        };
        Self {
            decoder,
            wait_for_key: true,
        }
    }

    fn on_unit(
        &mut self,
        core: &RxCore,
        shared: &VideoShared,
        egui_ctx: &eframe::egui::Context,
        unit: &DecodeUnit,
    ) {
        let Some(dec) = self.decoder.as_mut() else {
            return;
        };
        if self.wait_for_key && !unit.is_key {
            // Mid-GOP join (reconnect onto a resumed Sunshine session):
            // deltas keep flowing and nothing else ever asks for a key, so
            // without this latch the session shows white forever (field
            // report 2026-07-15). request_idr collapses into the next
            // session tick's needs-IDR ack — safe to latch per skipped unit.
            core.request_idr();
            return;
        }
        self.wait_for_key = false;
        // Opt-in GPU NV12 present: only when the raw D3D11 path and hardware
        // decode are active (software decode yields YUV420P, not NV12). Any
        // decode error falls through to the shared IDR-request handling.
        // Sharpen runs only in the RGBA present path (draw_sharpen); the
        // NV12 fast path renders its own NV12→RGB shader straight to the
        // backbuffer with no sharpen pass (field report 2026-07-16:
        // "sharpen stopped working" with NV12 on). While sharpen is
        // non-zero, fall back to the RGBA path so the slider always works.
        let want_nv12 = shared.nv12.load(Ordering::Relaxed)
            && shared.raw_present_active()
            && shared.hw_device.load(Ordering::Relaxed)
            && shared.sharpen_pct.load(Ordering::Relaxed) == 0;
        let decoded = if want_nv12 {
            dec.decode_nv12(&unit.data)
                .map(|o| o.map(video::DecodedFrame::Nv12))
        } else {
            dec.decode(&unit.data)
                .map(|o| o.map(video::DecodedFrame::Rgba))
        };
        match decoded {
            Ok(Some(frame)) => {
                let (w, h) = frame.dims();
                shared
                    .dims
                    .store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
                *shared.frame.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
                shared.generation.fetch_add(1, Ordering::Release);
                shared.decoded.fetch_add(1, Ordering::Relaxed);
                shared.frame_ready.notify_all();
                if !shared.raw_present_active() {
                    // The egui fallback only repaints on demand; the raw
                    // thread presents without waking the chrome.
                    egui_ctx.request_repaint();
                }
            }
            Ok(None) => {}
            Err(e) => {
                shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                self.wait_for_key = true;
                core.request_idr();
                tracing::warn!(err = %e, frame_id = unit.frame_id, "decode failed — requesting IDR");
            }
        }
    }

    /// Advance the decoder over a stale unit without presenting it (skip
    /// backlog): same IDR gating as [`Self::on_unit`], but the picture is
    /// decoded and dropped instead of converted + published.
    fn drop_unit(&mut self, core: &RxCore, shared: &VideoShared, unit: &DecodeUnit) {
        let Some(dec) = self.decoder.as_mut() else {
            return;
        };
        if self.wait_for_key && !unit.is_key {
            core.request_idr();
            return;
        }
        self.wait_for_key = false;
        if let Err(e) = dec.decode_drop(&unit.data) {
            shared.decode_errors.fetch_add(1, Ordering::Relaxed);
            self.wait_for_key = true;
            core.request_idr();
            tracing::warn!(err = %e, frame_id = unit.frame_id, "decode(drop) failed — requesting IDR");
        }
    }
}

struct Running {
    session: Session,
    core: Arc<RxCore>,
    stats: Arc<RxStats>,
    fps: Arc<FpsWindow>,
    started: Instant,
    pump: Option<std::thread::JoinHandle<()>>,
    #[cfg(feature = "video")]
    video: Arc<VideoShared>,
    /// Opus → WASAPI shared render thread (slice 4).
    #[cfg(all(windows, feature = "video"))]
    audio: Arc<audio::AudioShared>,
    #[cfg(all(windows, feature = "video"))]
    audio_thread: Option<std::thread::JoinHandle<()>>,
}

/// Effective session stream parameters (G005 S1b): derived from the
/// settings store via `settings::to_flowconfig_fields`, folded into
/// `FlowConfig` at connect time. Replaces the previous hardcoded
/// bitrate_kbps=8000/width=1920/height=1080/fps=60.
struct StreamFields {
    bitrate_kbps: u32,
    width: u32,
    height: u32,
    fps: u32,
    supported_codecs: u32,
}

impl Running {
    fn start(
        cfg: ConnectForm,
        audio_exclusive: bool,
        stream: StreamFields,
        egui_ctx: eframe::egui::Context,
    ) -> Self {
        #[cfg(not(all(windows, feature = "video")))]
        let _ = audio_exclusive;
        let core = Arc::new(RxCore::new(0));
        let stats = Arc::new(RxStats::default());
        let fps = Arc::new(FpsWindow::default());
        let started = Instant::now();
        #[cfg(feature = "video")]
        let video_shared = Arc::new(VideoShared::default());

        let session = Session::start(
            SessionConfig {
                base_url: cfg.base_url.trim_end_matches('/').to_string(),
                username: cfg.username,
                password: cfg.password,
                // A0: dev trust. Shipping path = SHA-256 pin (tls.rs);
                // surfaced in the UI before any non-localhost default.
                trust: ServerTrust::InsecureAcceptAny,
                flow: FlowConfig {
                    host_id: cfg.host_id,
                    app_id: cfg.app_id,
                    video_frame_queue_size: 3,
                    audio_sample_queue_size: 20,
                    bitrate_kbps: stream.bitrate_kbps,
                    width: stream.width,
                    height: stream.height,
                    fps: stream.fps,
                    supported_codecs: stream.supported_codecs,
                },
            },
            core.clone(),
        );

        // Frame pump == the decoder thread: pulls complete access units
        // from the shared RxCore. With the `video` feature each unit goes
        // through FFmpeg (D3D11VA, sw fallback) and the newest picture is
        // published for the UI; without it, frames are counted and dropped.
        let pump = {
            let core = core.clone();
            let stats = stats.clone();
            let fps = fps.clone();
            #[cfg(feature = "video")]
            let video = video_shared.clone();
            std::thread::Builder::new()
                .name("a0-frame-pump".into())
                .spawn(move || {
                    #[cfg(feature = "video")]
                    let mut decode = DecodeState::new(&video);
                    #[cfg(not(feature = "video"))]
                    let _ = &egui_ctx;
                    loop {
                        match core.wait_frame(Duration::from_millis(250)) {
                            Some(unit) => {
                                let now = Instant::now();
                                #[cfg(feature = "video")]
                                {
                                    let account = |u: &DecodeUnit| {
                                        stats.frames.fetch_add(1, Ordering::Relaxed);
                                        if u.is_key {
                                            stats.key_frames.fetch_add(1, Ordering::Relaxed);
                                        }
                                        stats
                                            .payload_bytes
                                            .fetch_add(u.data.len() as u64, Ordering::Relaxed);
                                    };
                                    account(&unit);
                                    // Skip stale backlog: decode older queued
                                    // units to keep the reference chain current
                                    // but only convert + present the newest, so
                                    // presentation latency never accumulates when
                                    // download/convert briefly falls behind 60 fps.
                                    let mut newest = unit;
                                    while let Some(next) = core.try_frame() {
                                        account(&next);
                                        decode.drop_unit(&core, &video, &newest);
                                        newest = next;
                                    }
                                    stats.last_frame_ms.store(
                                        now.duration_since(started).as_millis() as u64,
                                        Ordering::Relaxed,
                                    );
                                    fps.push(now);
                                    decode.on_unit(&core, &video, &egui_ctx, &newest);
                                }
                                #[cfg(not(feature = "video"))]
                                {
                                    stats.frames.fetch_add(1, Ordering::Relaxed);
                                    if unit.is_key {
                                        stats.key_frames.fetch_add(1, Ordering::Relaxed);
                                    }
                                    stats
                                        .payload_bytes
                                        .fetch_add(unit.data.len() as u64, Ordering::Relaxed);
                                    stats.last_frame_ms.store(
                                        now.duration_since(started).as_millis() as u64,
                                        Ordering::Relaxed,
                                    );
                                    fps.push(now);
                                }
                            }
                            None => {
                                if stats.stopped.load(Ordering::Acquire) {
                                    return;
                                }
                            }
                        }
                    }
                })
                .expect("spawn frame pump")
        };

        #[cfg(all(windows, feature = "video"))]
        let audio_shared = Arc::new(audio::AudioShared::default());
        #[cfg(all(windows, feature = "video"))]
        audio_shared
            .exclusive
            .store(audio_exclusive, Ordering::Relaxed);
        #[cfg(all(windows, feature = "video"))]
        let audio_thread = {
            let core = core.clone();
            let shared = audio_shared.clone();
            let stats = stats.clone();
            std::thread::Builder::new()
                .name("a0-audio".into())
                .spawn(move || audio::run(&core, &shared, &stats.stopped))
                .expect("spawn audio thread")
        };

        Self {
            session,
            core,
            stats,
            fps,
            started,
            pump: Some(pump),
            #[cfg(feature = "video")]
            video: video_shared,
            #[cfg(all(windows, feature = "video"))]
            audio: audio_shared,
            #[cfg(all(windows, feature = "video"))]
            audio_thread: Some(audio_thread),
        }
    }

    fn stop(mut self) {
        self.stats.stopped.store(true, Ordering::Release);
        self.core.close(); // unblocks the pump and the audio thread
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
        #[cfg(all(windows, feature = "video"))]
        if let Some(a) = self.audio_thread.take() {
            let _ = a.join();
        }
        self.session.stop();
    }
}

// ── App ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ConnectForm {
    base_url: String,
    username: String,
    password: String,
    host_id: u32,
    app_id: u32,
}

/// Deployment defaults from a `betterparsec.conf` (simple `key=value` lines)
/// sitting next to the exe, written by `tools/package-portable.ps1`. Lets a
/// double-clicked portable build pre-fill the connect form (Parsec-style
/// zero-config: download, run, type only the password) without env vars or
/// the launch `.bat`. Missing file → empty map; env vars still override.
fn packaged_conf() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("betterparsec.conf")))
    else {
        return map;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return map;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

impl Default for ConnectForm {
    fn default() -> Self {
        let conf = packaged_conf();
        // Precedence: env var (dev override) > betterparsec.conf (packaged
        // deployment defaults) > built-in fallback. The password is never
        // baked — it is the user's account secret, typed at connect time.
        let text = |env: &str, key: &str, fallback: &str| -> String {
            std::env::var(env)
                .ok()
                .or_else(|| conf.get(key).cloned())
                .unwrap_or_else(|| fallback.to_string())
        };
        let id = |env: &str, key: &str| -> u32 {
            std::env::var(env)
                .ok()
                .or_else(|| conf.get(key).cloned())
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0)
        };
        Self {
            base_url: text("BP_URL", "base_url", "https://localhost:8080"),
            username: text("BP_USER", "username", ""),
            password: std::env::var("BP_PASS").unwrap_or_default(),
            host_id: id("BP_HOST_ID", "host_id"),
            app_id: id("BP_APP_ID", "app_id"),
        }
    }
}

struct App {
    form: ConnectForm,
    /// G005 S1b user settings store (`%APPDATA%/betterparsec/settings.json`).
    /// Precedence slot: env(BP_*) > betterparsec.conf > THIS store >
    /// built-in defaults. Loaded once at startup, saved on every sidebar
    /// edit.
    settings: settings::Settings,
    host_id_text: String,
    app_id_text: String,
    running: Option<Running>,
    /// Host role (D1): embedded web-server; independent of the client
    /// session ("both" is the LAN-party topology).
    host: Option<host::Host>,
    host_error: Option<String>,
    /// Uploaded stream texture (egui fallback present, slice 2).
    #[cfg(feature = "video")]
    video_tex: Option<eframe::egui::TextureHandle>,
    /// Generation of the frame currently in `video_tex`.
    #[cfg(feature = "video")]
    video_gen: u64,
    /// Raw D3D11 stream surface (slice 3) — child HWND + render thread.
    #[cfg(all(windows, feature = "video"))]
    surface: Option<present::StreamSurface>,
    /// Raw surface init/present failed this connection — egui fallback.
    #[cfg(all(windows, feature = "video"))]
    surface_failed: bool,
    /// M4 cursor P2 (opt-in `BP_CLIENT_CURSOR=1`): client-rendered host
    /// cursor state (HCURSOR ring + wndproc-shared slot, cursor_icon.rs).
    #[cfg(all(windows, feature = "video"))]
    client_cursor: bool,
    #[cfg(all(windows, feature = "video"))]
    cursor_state: cursor_icon::ClientCursor,
    /// Immersive mode (M4 Phase B): fullscreen + RawInput relative mouse
    /// + cursor clip, one toggle (immersive.rs machine).
    #[cfg(all(windows, feature = "video"))]
    immersive: immersive::Immersive,
    #[cfg(all(windows, feature = "video"))]
    capture: std::sync::Arc<input::CaptureShared>,
    /// Client-side sharpen strength (0..100 percent), live UI slider.
    /// Seeded from `BP_SHARPEN`; pushed to `VideoShared::sharpen_pct` each
    /// frame so the present thread applies it (present.rs sharpen pass).
    #[cfg(feature = "video")]
    sharpen_pct: u32,
    /// Opt-in WASAPI exclusive audio (low latency). Seeded from
    /// `BP_AUDIO_EXCLUSIVE`; the sidebar toggle persists it here so it
    /// applies on the next connect (seeded into `AudioShared::exclusive`
    /// at `Running::start` — the device is owned per-session, so it
    /// cannot flip live).
    #[cfg(all(windows, feature = "video"))]
    audio_exclusive: bool,
}

impl App {
    fn new() -> Self {
        let form = ConnectForm::default();
        let settings = settings::load();
        #[cfg(all(windows, feature = "video"))]
        let client_cursor_pref = std::env::var("BP_CLIENT_CURSOR").is_ok_and(|v| v == "1")
            || settings.client.client_cursor;
        Self {
            host_id_text: form.host_id.to_string(),
            app_id_text: form.app_id.to_string(),
            form,
            settings,
            running: None,
            host: None,
            host_error: None,
            #[cfg(feature = "video")]
            video_tex: None,
            #[cfg(feature = "video")]
            video_gen: 0,
            #[cfg(all(windows, feature = "video"))]
            surface: None,
            #[cfg(all(windows, feature = "video"))]
            surface_failed: false,
            #[cfg(all(windows, feature = "video"))]
            client_cursor: client_cursor_pref,
            #[cfg(all(windows, feature = "video"))]
            cursor_state: cursor_icon::ClientCursor::default(),
            #[cfg(all(windows, feature = "video"))]
            immersive: immersive::Immersive::default(),
            #[cfg(all(windows, feature = "video"))]
            capture: std::sync::Arc::default(),
            #[cfg(feature = "video")]
            sharpen_pct: std::env::var("BP_SHARPEN")
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .unwrap_or(0)
                .min(100),
            #[cfg(all(windows, feature = "video"))]
            audio_exclusive: std::env::var("BP_AUDIO_EXCLUSIVE").as_deref() == Ok("1"),
        }
    }

    /// Persisted WASAPI-exclusive audio preference (windows+video only;
    /// always `false` otherwise so `Running::start` has a value to seed).
    #[cfg(all(windows, feature = "video"))]
    fn audio_exclusive_pref(&self) -> bool {
        self.audio_exclusive
    }
    #[cfg(not(all(windows, feature = "video")))]
    fn audio_exclusive_pref(&self) -> bool {
        false
    }

    /// G005 S1b: `FlowConfig`'s bitrate/width/height/fps/codecs, derived
    /// from the settings store through the pure mode engine.
    fn stream_fields(&self) -> StreamFields {
        let (bitrate_kbps, width, height, fps, supported_codecs) =
            settings::to_flowconfig_fields(&self.settings.client);
        StreamFields {
            bitrate_kbps,
            width,
            height,
            fps,
            supported_codecs,
        }
    }

    /// G005 S1b sidebar: 3-way mode selector + bitrate/resolution/fps
    /// overrides + the `BP_PRESENT_10BIT`-folding store toggle. Any change
    /// updates the in-memory store and persists it immediately
    /// (`settings::save`) — next connect (or a live reconnect) picks it up
    /// via `stream_fields`/`want_10bit_pref`.
    fn settings_panel(&mut self, ui: &mut eframe::egui::Ui) {
        use eframe::egui;
        let mut changed = false;
        egui::CollapsingHeader::new("Stream settings")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Mode");
                    egui::ComboBox::from_id_salt("stream-mode")
                        .selected_text(match self.settings.client.mode {
                            transport_core::mode::StreamMode::Fast => "Fast",
                            transport_core::mode::StreamMode::Medium => "Medium",
                            transport_core::mode::StreamMode::Quality => "Quality",
                        })
                        .show_ui(ui, |ui| {
                            for (label, mode) in [
                                ("Fast", transport_core::mode::StreamMode::Fast),
                                ("Medium", transport_core::mode::StreamMode::Medium),
                                ("Quality", transport_core::mode::StreamMode::Quality),
                            ] {
                                if ui
                                    .selectable_value(&mut self.settings.client.mode, mode, label)
                                    .changed()
                                {
                                    changed = true;
                                }
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Bitrate (kbps, 0 = mode default)");
                    changed |= ui
                        .add(egui::DragValue::new(&mut self.settings.client.bitrate_kbps))
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("Resolution (0x0 = mode default)");
                    changed |= ui
                        .add(egui::DragValue::new(&mut self.settings.client.width))
                        .changed();
                    ui.label("x");
                    changed |= ui
                        .add(egui::DragValue::new(&mut self.settings.client.height))
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("FPS (0 = mode default)");
                    changed |= ui
                        .add(egui::DragValue::new(&mut self.settings.client.fps))
                        .changed();
                });
                changed |= ui
                    .checkbox(
                        &mut self.settings.client.present_10bit,
                        "10-bit present (or BP_PRESENT_10BIT=1)",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.settings.client.client_cursor,
                        "Client-rendered cursor (or BP_CLIENT_CURSOR=1)",
                    )
                    .changed();
            });
        if changed && let Err(e) = settings::save(&self.settings) {
            tracing::warn!(err = %e, "settings store save failed");
        }
        // S1d: the live per-session flag follows the store, with the
        // BP_CLIENT_CURSOR env override folded on top (dev-only, highest).
        #[cfg(all(windows, feature = "video"))]
        {
            self.client_cursor = std::env::var("BP_CLIENT_CURSOR").is_ok_and(|v| v == "1")
                || self.settings.client.client_cursor;
        }
    }

    /// G005 S1b: the raw-present `want_10bit` — the store preference; the
    /// `BP_PRESENT_10BIT` env override is folded in by `present.rs` itself
    /// (`Renderer::new_with_10bit`), so only the store half travels here.
    #[cfg(all(windows, feature = "video"))]
    fn want_10bit_pref(&self) -> bool {
        self.settings.client.present_10bit
    }

    /// Session teardown half of the immersive machine: run the owed
    /// actions with the surface possibly already gone (global capture
    /// release is surface-independent).
    #[cfg(all(windows, feature = "video"))]
    fn reset_immersive(&mut self, ctx: &eframe::egui::Context) {
        for action in self.immersive.reset() {
            match action {
                immersive::Action::SetFullscreen(on) => {
                    ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Fullscreen(on))
                }
                immersive::Action::Release => {
                    self.capture.set_relative(false);
                    self.capture.set_keyboard_capture(false);
                    present::release_mouse_capture_global();
                }
                immersive::Action::Engage => {} // reset never engages
            }
        }
    }

    /// Host role strip (D1): start/stop the embedded web-server. Rendered
    /// on every screen — hosting and a client session may run together.
    fn host_section(&mut self, ui: &mut eframe::egui::Ui) {
        let mut start_clicked = false;
        let mut stop_clicked = false;
        ui.horizontal(|ui| match &mut self.host {
            None => {
                start_clicked = ui.button("Start host").clicked();
                ui.small("embedded web-server (accounts/pairing/signaling)");
            }
            Some(h) => {
                stop_clicked = ui.button("Stop host").clicked();
                let sunshine = match (&mut h.sunshine, &h.sunshine_error) {
                    (Some(s), _) => {
                        if s.is_running() {
                            format!("sunshine pid {} port {}", s.pid(), s.port)
                        } else {
                            "sunshine EXITED — stop/start host".into()
                        }
                    }
                    (None, Some(_)) => "sunshine FAILED (see below)".into(),
                    (None, None) => "no sunshine (set BP_SUNSHINE_STAGE)".into(),
                };
                ui.label(format!(
                    "hosting on {} ({}) — {}",
                    h.server
                        .addrs()
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    match h.config_source {
                        host::ConfigSource::File => format!("config: {}", h.config_path.display()),
                        host::ConfigSource::BuiltinDefault => "default config".into(),
                    },
                    sunshine,
                ));
            }
        });
        if let Some(h) = &self.host
            && let Some(e) = &h.sunshine_error
        {
            ui.colored_label(eframe::egui::Color32::YELLOW, format!("sunshine: {e}"));
        }
        if start_clicked {
            match host::start(std::path::Path::new(host::DEFAULT_CONFIG_PATH)) {
                Ok(h) => {
                    self.host = Some(h);
                    self.host_error = None;
                }
                Err(e) => {
                    tracing::error!(err = %e, "host role start failed");
                    self.host_error = Some(e);
                }
            }
        }
        if stop_clicked && let Some(h) = self.host.take() {
            h.stop();
        }
        if let Some(e) = &self.host_error {
            ui.colored_label(eframe::egui::Color32::RED, format!("host: {e}"));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        use eframe::egui;

        #[cfg(not(all(windows, feature = "video")))]
        let _ = &frame;

        // Live counters need continuous repaint while connected.
        if self.running.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("BetterParsec");
            ui.add_space(8.0);
            self.host_section(ui);
            ui.separator();

            match &self.running {
                None => {
                    egui::Grid::new("connect-form")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Server");
                            ui.text_edit_singleline(&mut self.form.base_url);
                            ui.end_row();
                            ui.label("User");
                            ui.text_edit_singleline(&mut self.form.username);
                            ui.end_row();
                            ui.label("Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.password).password(true),
                            );
                            ui.end_row();
                            ui.label("Host ID");
                            ui.text_edit_singleline(&mut self.host_id_text);
                            ui.end_row();
                            ui.label("App ID");
                            ui.text_edit_singleline(&mut self.app_id_text);
                            ui.end_row();
                        });
                    ui.add_space(4.0);
                    self.settings_panel(ui);
                    ui.add_space(8.0);
                    if ui.button("Connect").clicked() {
                        self.form.host_id = self.host_id_text.trim().parse().unwrap_or(0);
                        self.form.app_id = self.app_id_text.trim().parse().unwrap_or(0);
                        // Fresh connection = fresh chance for the raw
                        // surface (the latch is per-connection, not per-app).
                        #[cfg(all(windows, feature = "video"))]
                        {
                            self.surface_failed = false;
                            self.cursor_state.reset(); // per-connection shapes
                            self.reset_immersive(ctx);
                        }
                        self.running = Some(Running::start(
                            self.form.clone(),
                            self.audio_exclusive_pref(),
                            self.stream_fields(),
                            ctx.clone(),
                        ));
                    }
                    ui.add_space(4.0);
                    ui.small("dev TLS: accepts any certificate (localhost testing)");
                }
                Some(run) => {
                    let state = run.session.state();
                    // Hold watchdog escalation while minimized (parity
                    // with the web wiring's document-hidden pause).
                    run.session
                        .set_watchdog_paused(ctx.input(|i| i.viewport().minimized.unwrap_or(false)));
                    // Terminal watchdog rung: the session ended itself
                    // after the full ladder — rebuild below (web parity).
                    let watchdog_reconnect = state == SessionState::Failed
                        && run.session.watchdog().reconnect_requested();
                    let frames = run.stats.frames.load(Ordering::Relaxed);
                    let keys = run.stats.key_frames.load(Ordering::Relaxed);
                    let bytes = run.stats.payload_bytes.load(Ordering::Relaxed);
                    let last_ms = run.stats.last_frame_ms.load(Ordering::Relaxed);
                    let uptime = run.started.elapsed().as_secs_f64();
                    let stalled_for = if last_ms > 0 {
                        uptime - (last_ms as f64 / 1000.0)
                    } else {
                        uptime
                    };

                    ui.label(format!("state: {state:?}"));
                    if let Some(p) = run.session.stream_params() {
                        ui.label(format!(
                            "stream: {}x{}@{} format=0x{:x} audio {} Hz ch{}",
                            p.width,
                            p.height,
                            p.fps,
                            p.format,
                            p.audio_sample_rate,
                            p.audio_channel_count
                        ));
                    }
                    ui.separator();
                    ui.label(format!("frames: {frames}  (key: {keys})"));
                    ui.label(format!("fps (2s window): {:.1}", run.fps.fps()));
                    ui.label(format!("payload: {:.2} MB", bytes as f64 / 1e6));
                    // Stall indicator: readback of the session-embedded
                    // watchdog ladder (M4 field issue #1, client-transport
                    // watchdog.rs); seconds from the shell-side pump stats.
                    if run.session.watchdog().stalled() {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            format!("stream stalled — recovering ({stalled_for:.1}s)"),
                        );
                    }
                    if state == SessionState::Failed {
                        ui.colored_label(egui::Color32::RED, "session failed — see log");
                    }
                    #[cfg(feature = "video")]
                    {
                        ui.separator();
                        ui.label(format!(
                            "decoded: {} ({}, errors: {}) — {}",
                            run.video.decoded.load(Ordering::Relaxed),
                            if run.video.hw_device.load(Ordering::Relaxed) {
                                "d3d11va"
                            } else {
                                "sw decode"
                            },
                            run.video.decode_errors.load(Ordering::Relaxed),
                            if run.video.raw_present_active() {
                                "raw flip present"
                            } else {
                                "egui fallback present"
                            },
                        ));
                        #[cfg(windows)]
                        ui.label(format!(
                            "audio: {} (packets: {}, errors: {})",
                            match run.audio.state.load(Ordering::Acquire) {
                                audio::AUDIO_RUNNING => "wasapi shared",
                                audio::AUDIO_FAILED => "off — see log",
                                _ => "starting",
                            },
                            run.audio.packets.load(Ordering::Relaxed),
                            run.audio.decode_errors.load(Ordering::Relaxed),
                        ));
                    }

                    // Client-side sharpen (present.rs CAS/unsharp pass):
                    // a live slider so it's adjustable without env/batch.
                    #[cfg(feature = "video")]
                    {
                        ui.horizontal(|ui| {
                            ui.label("sharpen");
                            ui.add(egui::Slider::new(&mut self.sharpen_pct, 0..=100).suffix("%"));
                        });
                        run.video
                            .sharpen_pct
                            .store(self.sharpen_pct, Ordering::Relaxed);
                        // Opt-in GPU NV12 present (skips the CPU color
                        // roundtrip); live toggle, default off (proven RGBA).
                        let mut nv12 = run.video.nv12.load(Ordering::Relaxed);
                        if ui
                            .checkbox(&mut nv12, "Fast GPU present (NV12, experimental)")
                            .changed()
                        {
                            run.video.nv12.store(nv12, Ordering::Relaxed);
                        }
                    }
                    // Opt-in WASAPI exclusive audio (low latency): the
                    // render device is owned for the session's lifetime, so
                    // the audio thread latches its mode at connect — this
                    // toggle takes effect on the next connect/reconnect.
                    #[cfg(all(windows, feature = "video"))]
                    {
                        ui.checkbox(
                            &mut self.audio_exclusive,
                            "Exclusive audio (low latency, reconnect to apply)",
                        );
                    }
                    ui.add_space(8.0);
                    let disconnect = ui.button("Disconnect").clicked();
                    // Hard-disconnect hotkey (Ctrl+Alt+`, input.rs):
                    // consumed once per frame; rides the same teardown
                    // as the button — works from fullscreen immersive.
                    #[cfg(all(windows, feature = "video"))]
                    let disconnect = disconnect || self.capture.take_disconnect_requested();
                    // M4 Phase B: one toggle = fullscreen + RawInput
                    // relative mouse + cursor clip (immersive.rs).
                    #[cfg(all(windows, feature = "video"))]
                    let immersive_clicked = ui
                        .button(if self.immersive.engaged() {
                            "Exit immersive"
                        } else {
                            "Immersive"
                        })
                        .clicked();
                    ui.add_space(4.0);
                    #[cfg(feature = "video")]
                    ui.small("A0/A2: FFmpeg decode + raw D3D11 surface + WASAPI audio + mouse/keyboard to host");
                    #[cfg(not(feature = "video"))]
                    ui.small("built without the `video` feature — frames are received and counted only");

                    // Everything below the chrome is the stream viewport.
                    #[cfg(feature = "video")]
                    {
                        let avail = ui.available_size();
                        if avail.y > 8.0 {
                            let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());

                            // Raw D3D11 child surface (slice 3): create
                            // lazily, keep its HWND aspect-fit inside the
                            // viewport, demote to the egui fallback on
                            // failure.
                            #[cfg(windows)]
                            if !self.surface_failed {
                                if self.surface.is_none() {
                                    match parent_hwnd(frame) {
                                        Some(parent) => match present::StreamSurface::create(
                                            parent,
                                            run.video.clone(),
                                            self.want_10bit_pref(),
                                        ) {
                                            Ok(mut s) => {
                                                // A2: mouse/keyboard over the
                                                // stream go to the host.
                                                s.enable_input(input::InputCtx {
                                                    sender: run.session.input_sender(),
                                                    video: run.video.clone(),
                                                    cursor: self.cursor_state.active_slot(),
                                                    capture: self.capture.clone(),
                                                });
                                                self.surface = Some(s);
                                            }
                                            Err(e) => {
                                                tracing::error!(err = %e, "stream surface create failed — egui fallback");
                                                run.video
                                                    .raw_present_failed
                                                    .store(true, Ordering::Release);
                                                self.surface_failed = true;
                                            }
                                        },
                                        None => {
                                            tracing::error!("no Win32 window handle — egui fallback");
                                            run.video
                                                .raw_present_failed
                                                .store(true, Ordering::Release);
                                            self.surface_failed = true;
                                        }
                                    }
                                }
                                match self.surface.as_mut() {
                                    Some(s) if s.failed() => {
                                        // Present thread latched fallback
                                        // (raw_present_failed is set, so the
                                        // egui texture path takes over).
                                        tracing::warn!(
                                            "stream surface failed — egui fallback for this connection"
                                        );
                                        self.surface = None;
                                        self.surface_failed = true;
                                    }
                                    Some(s) => {
                                        let dims = run.video.dims.load(Ordering::Relaxed);
                                        let (vw, vh) =
                                            ((dims >> 32) as f32, (dims & 0xffff_ffff) as f32);
                                        // Immersive: the stream child covers
                                        // the whole window (over the egui
                                        // chrome) so the picture is truly
                                        // fullscreen and the cursor clip spans
                                        // the entire screen. Otherwise it is
                                        // aspect-fit into the viewport rect
                                        // below the chrome.
                                        let target = if self.immersive.engaged() {
                                            ctx.screen_rect()
                                        } else {
                                            rect
                                        };
                                        let fit = if vw > 0.0 && vh > 0.0 {
                                            let k = (target.width() / vw)
                                                .min(target.height() / vh);
                                            egui::Rect::from_center_size(
                                                target.center(),
                                                egui::vec2(vw * k, vh * k),
                                            )
                                        } else {
                                            target
                                        };
                                        let ppp = ctx.pixels_per_point();
                                        s.set_rect(
                                            (fit.min.x * ppp).round() as i32,
                                            (fit.min.y * ppp).round() as i32,
                                            (fit.width() * ppp).round() as i32,
                                            (fit.height() * ppp).round() as i32,
                                        );
                                        // Single-cursor, parent half: while
                                        // the chrome window has focus winit
                                        // re-applies its cursor every frame,
                                        // overriding the child's
                                        // WM_SETCURSOR hide — report None to
                                        // egui while the pointer is over the
                                        // stream child (field issue #2).
                                        if s.cursor_over() {
                                            ctx.set_cursor_icon(egui::CursorIcon::None);
                                        }
                                        // M4 cursor P2 pump: newest host
                                        // shape → HCURSOR for the child's
                                        // WM_SETCURSOR (cursor_icon.rs).
                                        if self.client_cursor {
                                            self.cursor_state.pump(run.session.cursor());
                                        }
                                        // M4 Phase B: immersive machine —
                                        // one settle frame then engage; exits
                                        // converge on one Release. Capture no
                                        // longer waits on the OS fullscreen/
                                        // focus readback (it could wedge
                                        // Entering forever — immersive.rs).
                                        let exit_requested =
                                            self.capture.take_exit_requested();
                                        for action in self
                                            .immersive
                                            .on_tick(immersive_clicked || exit_requested)
                                        {
                                            match action {
                                                immersive::Action::SetFullscreen(on) => {
                                                    ctx.send_viewport_cmd(
                                                        egui::ViewportCommand::Fullscreen(on),
                                                    )
                                                }
                                                immersive::Action::Engage => {
                                                    // Phase B2: seed the
                                                    // initial mouse mode
                                                    // from host cursor
                                                    // authority rather
                                                    // than always-relative.
                                                    self.capture.set_relative(
                                                        immersive::wants_relative_capture(
                                                            true,
                                                            run.session.cursor().visible(),
                                                        ),
                                                    );
                                                    self.capture.set_keyboard_capture(true);
                                                    s.engage_mouse_capture(
                                                        self.capture.clone(),
                                                        run.session.input_sender(),
                                                    );
                                                }
                                                immersive::Action::Release => {
                                                    self.capture.set_relative(false);
                                                    self.capture.set_keyboard_capture(false);
                                                    s.release_mouse_capture();
                                                    // Clear any host-latched
                                                    // modifiers on exit (stuck
                                                    // Alt — input.rs).
                                                    input::release_sticky_keys(
                                                        &run.session.input_sender(),
                                                    );
                                                }
                                            }
                                        }
                                        if self.immersive.engaged() {
                                            // Phase B2 host-authority
                                            // auto-switch (Parsec/Moonlight
                                            // parity): mirror the host's
                                            // reported cursor visibility
                                            // every frame — fullscreen and
                                            // the keyboard hook stay
                                            // engaged for the whole
                                            // session, only the mouse
                                            // relative flag + clip follow
                                            // the host.
                                            let want_rel = immersive::wants_relative_capture(
                                                true,
                                                run.session.cursor().visible(),
                                            );
                                            self.capture.set_relative(want_rel);
                                            if want_rel {
                                                s.clip_cursor_to_self();
                                            } else {
                                                s.release_cursor_clip();
                                            }
                                        }
                                    }
                                    None => {}
                                }
                            }

                            // Interim egui texture present (slice 2):
                            // non-Windows builds and raw-path failure.
                            if !run.video.raw_present_active() {
                                let generation = run.video.generation.load(Ordering::Acquire);
                                if generation != self.video_gen
                                    && let Some(f) = run
                                        .video
                                        .frame
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner)
                                        .take()
                                {
                                    // NV12 only occurs while the raw present
                                    // path is active, so the egui fallback only
                                    // ever sees RGBA here; skip other forms.
                                    if let video::DecodedFrame::Rgba(r) = &f {
                                        let img = egui::ColorImage::from_rgba_unmultiplied(
                                            [r.width, r.height],
                                            &r.rgba,
                                        );
                                        match self.video_tex.as_mut() {
                                            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                                            None => {
                                                self.video_tex = Some(ctx.load_texture(
                                                    "stream",
                                                    img,
                                                    egui::TextureOptions::LINEAR,
                                                ));
                                            }
                                        }
                                    }
                                    self.video_gen = generation;
                                }
                                if let Some(tex) = &self.video_tex {
                                    let size = tex.size_vec2();
                                    let k = (rect.width() / size.x).min(rect.height() / size.y);
                                    let img_rect =
                                        egui::Rect::from_center_size(rect.center(), size * k);
                                    ui.painter().image(
                                        tex.id(),
                                        img_rect,
                                        egui::Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        egui::Color32::WHITE,
                                    );
                                }
                            }
                        }
                    }

                    if disconnect && let Some(run) = self.running.take() {
                        #[cfg(all(windows, feature = "video"))]
                        {
                            self.surface = None; // joins the present thread
                            self.cursor_state.reset();
                            self.reset_immersive(ctx);
                        }
                        run.stop();
                        #[cfg(feature = "video")]
                        {
                            self.video_tex = None;
                            self.video_gen = 0;
                        }
                    }

                    if watchdog_reconnect && let Some(run) = self.running.take() {
                        #[cfg(all(windows, feature = "video"))]
                        {
                            self.surface = None; // joins the present thread
                            self.surface_failed = false; // per-connection latch
                            self.cursor_state.reset();
                            self.reset_immersive(ctx);
                        }
                        run.stop();
                        #[cfg(feature = "video")]
                        {
                            self.video_tex = None;
                            self.video_gen = 0;
                        }
                        tracing::info!("stall watchdog reconnect: rebuilding the session");
                        self.running = Some(Running::start(
                            self.form.clone(),
                            self.audio_exclusive_pref(),
                            self.stream_fields(),
                            ctx.clone(),
                        ));
                    }
                }
            }
        });
    }
}

/// The eframe chrome window's Win32 handle (parent for the stream child).
#[cfg(all(windows, feature = "video"))]
fn parent_hwnd(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}
