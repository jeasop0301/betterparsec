//! BetterParsec unified app — A0 client first light
//! (docs/design/unified-app-architecture.md §6).
//!
//! This slice: native shell (egui chrome) + client-transport session +
//! live receive panel driven by the shared RxCore. The video surface
//! (raw D3D11 child HWND, FFmpeg D3D11VA decode) is the next A0 slice;
//! until then received frames are counted and dropped so the whole
//! session pipeline (signaling → WebRTC → video_fec → FEC decode →
//! FrameQueue) runs for real.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use client_transport::capi::RxCore;
use client_transport::flow::FlowConfig;
use client_transport::session::{H264_BIT, Session, SessionConfig, SessionState};
use client_transport::tls::ServerTrust;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,webrtc=warn,webrtc_ice=warn,webrtc_sctp=warn".into()),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([560.0, 480.0])
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

struct Running {
    session: Session,
    core: Arc<RxCore>,
    stats: Arc<RxStats>,
    fps: Arc<FpsWindow>,
    started: Instant,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn start(cfg: ConnectForm) -> Self {
        let core = Arc::new(RxCore::new(0));
        let stats = Arc::new(RxStats::default());
        let fps = Arc::new(FpsWindow::default());
        let started = Instant::now();

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

        // Decoder-thread stand-in: pull frames exactly like the future
        // D3D11VA pipeline will, count them, drop the bytes.
        let pump = {
            let core = core.clone();
            let stats = stats.clone();
            let fps = fps.clone();
            std::thread::Builder::new()
                .name("a0-frame-pump".into())
                .spawn(move || {
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
}

impl App {
    fn new() -> Self {
        let form = ConnectForm::default();
        Self {
            host_id_text: form.host_id.to_string(),
            app_id_text: form.app_id.to_string(),
            form,
            running: None,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        use eframe::egui;

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
                        self.running = Some(Running::start(self.form.clone()));
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
                    ui.add_space(8.0);
                    if ui.button("Disconnect").clicked()
                        && let Some(run) = self.running.take()
                    {
                        run.stop();
                    }
                    ui.add_space(4.0);
                    ui.small("A0: frames are received and counted; the D3D11 video surface is the next slice");
                }
            }
        });
    }
}
