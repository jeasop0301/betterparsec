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
mod present;
#[cfg(feature = "video")]
mod video;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(feature = "video")]
use std::sync::Condvar;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use client_transport::capi::RxCore;
use client_transport::flow::FlowConfig;
use client_transport::session::{H264_BIT, Session, SessionConfig, SessionState};
use client_transport::tls::ServerTrust;
#[cfg(feature = "video")]
use transport_core::video_rx::DecodeUnit;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,webrtc=warn,webrtc_ice=warn,webrtc_sctp=warn".into()),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_title("BetterParsec"),
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
    frame: Mutex<Option<video::RgbaFrame>>,
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
            return;
        }
        self.wait_for_key = false;
        match dec.decode(&unit.data) {
            Ok(Some(frame)) => {
                shared.dims.store(
                    ((frame.width as u64) << 32) | frame.height as u64,
                    Ordering::Relaxed,
                );
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
}

impl Running {
    fn start(cfg: ConnectForm, egui_ctx: eframe::egui::Context) -> Self {
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
                    bitrate_kbps: 8000,
                    width: 1920,
                    height: 1080,
                    fps: 60,
                    supported_codecs: H264_BIT,
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
                                #[cfg(feature = "video")]
                                decode.on_unit(&core, &video, &egui_ctx, &unit);
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

        Self {
            session,
            core,
            stats,
            fps,
            started,
            pump: Some(pump),
            #[cfg(feature = "video")]
            video: video_shared,
        }
    }

    fn stop(mut self) {
        self.stats.stopped.store(true, Ordering::Release);
        self.core.close(); // unblocks the pump
        if let Some(p) = self.pump.take() {
            let _ = p.join();
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

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            base_url: std::env::var("BP_URL").unwrap_or_else(|_| "https://localhost:8080".into()),
            username: std::env::var("BP_USER").unwrap_or_default(),
            password: std::env::var("BP_PASS").unwrap_or_default(),
            host_id: std::env::var("BP_HOST_ID")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            app_id: std::env::var("BP_APP_ID")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        }
    }
}

struct App {
    form: ConnectForm,
    host_id_text: String,
    app_id_text: String,
    running: Option<Running>,
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
}

impl App {
    fn new() -> Self {
        let form = ConnectForm::default();
        Self {
            host_id_text: form.host_id.to_string(),
            app_id_text: form.app_id.to_string(),
            form,
            running: None,
            #[cfg(feature = "video")]
            video_tex: None,
            #[cfg(feature = "video")]
            video_gen: 0,
            #[cfg(all(windows, feature = "video"))]
            surface: None,
            #[cfg(all(windows, feature = "video"))]
            surface_failed: false,
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
            ui.heading("BetterParsec — A0 client first light");
            ui.add_space(8.0);

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
                    ui.add_space(8.0);
                    if ui.button("Connect").clicked() {
                        self.form.host_id = self.host_id_text.trim().parse().unwrap_or(0);
                        self.form.app_id = self.app_id_text.trim().parse().unwrap_or(0);
                        self.running = Some(Running::start(self.form.clone(), ctx.clone()));
                    }
                    ui.add_space(4.0);
                    ui.small("dev TLS: accepts any certificate (localhost testing)");
                }
                Some(run) => {
                    let state = run.session.state();
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
                    // Poor-man's stall indicator until the session-ux
                    // watchdog lands (field issue #1).
                    if frames > 0 && stalled_for > 1.0 && state == SessionState::Streaming {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            format!("no frames for {stalled_for:.1}s"),
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
                    }
                    ui.add_space(8.0);
                    let disconnect = ui.button("Disconnect").clicked();
                    ui.add_space(4.0);
                    #[cfg(feature = "video")]
                    ui.small("A0 slice 3: FFmpeg decode + raw D3D11 FLIP_DISCARD stream surface");
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
                                        ) {
                                            Ok(s) => self.surface = Some(s),
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
                                        self.surface = None;
                                        self.surface_failed = true;
                                    }
                                    Some(s) => {
                                        let dims = run.video.dims.load(Ordering::Relaxed);
                                        let (vw, vh) =
                                            ((dims >> 32) as f32, (dims & 0xffff_ffff) as f32);
                                        let fit = if vw > 0.0 && vh > 0.0 {
                                            let k = (rect.width() / vw).min(rect.height() / vh);
                                            egui::Rect::from_center_size(
                                                rect.center(),
                                                egui::vec2(vw * k, vh * k),
                                            )
                                        } else {
                                            rect
                                        };
                                        let ppp = ctx.pixels_per_point();
                                        s.set_rect(
                                            (fit.min.x * ppp).round() as i32,
                                            (fit.min.y * ppp).round() as i32,
                                            (fit.width() * ppp).round() as i32,
                                            (fit.height() * ppp).round() as i32,
                                        );
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
                                    let img = egui::ColorImage::from_rgba_unmultiplied(
                                        [f.width, f.height],
                                        &f.rgba,
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
                        }
                        run.stop();
                        #[cfg(feature = "video")]
                        {
                            self.video_tex = None;
                            self.video_gen = 0;
                        }
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
