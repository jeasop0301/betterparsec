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
mod identity;
#[cfg(all(windows, feature = "video"))]
mod immersive;
#[cfg(all(windows, feature = "video"))]
mod input;
#[cfg(all(windows, feature = "video"))]
mod present;
mod settings;
mod sunshine;
mod supervisor;
mod update;
#[cfg(feature = "video")]
mod video;

#[cfg(feature = "video")]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use client_transport::capi::RxCore;
use client_transport::flow::FlowConfig;
use client_transport::frame_queue::VideoEvent;
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
    tracing::info!("=== betterparsec build 07-17b starting ===");

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_title("BetterParsec — build 07-17b (focus-guarded capture)"),
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
    /// G004: UI thread sets this when `Session::watchdog().decode_stall()`
    /// latches; the frame-pump thread swaps it back to `false` and applies
    /// [`DecodeState::on_decode_stall`] on the next loop iteration (cross-
    /// thread signal — the pump owns the live FFmpeg `Decoder`, the UI
    /// thread owns the watchdog readback).
    decode_stall_pending: AtomicBool,
    /// G004: same signal shape as `decode_stall_pending`, consumed by the
    /// raw D3D11 present thread via the bounded `PresentStallLadder`
    /// (`present.rs`).
    present_stall_pending: AtomicBool,
}

#[cfg(feature = "video")]
impl VideoShared {
    /// The raw D3D11 present path is (still) responsible for drawing.
    fn raw_present_active(&self) -> bool {
        cfg!(windows) && !self.raw_present_failed.load(Ordering::Acquire)
    }
}

/// Per-pump decoder state: FFmpeg decoder + generation-scoped key-frame
/// recovery.
#[cfg(feature = "video")]
struct DecodeState {
    decoder: Option<video::Decoder>,
    /// Armed while waiting for a decoded key to clear recovery, `None` once
    /// a real matching-epoch decoded key has cleared it. Armed on every
    /// `VideoEvent::Discontinuity` (which also flushes the decoder) and
    /// re-armed on a decode error; cleared *only* by [`Self::clears_recovery`]
    /// — a real matching-epoch decoded key — never by packet submission or
    /// presentation. A failed key packet, or a key packet FFmpeg swallows
    /// without emitting a picture, never produces a `DecodeMeta` and so can
    /// never reach that clear. The armed value is an epoch (see `epoch`
    /// below); the RxCore recovery this is meant to ack is additionally
    /// keyed by `recovery_generation`.
    wait_for_key: Option<u32>,
    /// RxCore recovery-generation this instance is currently tracking, or
    /// `None` before any transport discontinuity has ever fired (the
    /// initial mid-GOP-join gate has nothing to ack — see `on_meta`). Set
    /// alongside `wait_for_key` by [`Self::on_discontinuity`]; acking by
    /// generation (not epoch alone) is what stops an earlier same-epoch
    /// discontinuity's key from closing a later one's recovery.
    recovery_generation: Option<u64>,
    /// Latest known transport epoch, or `None` before the very first unit
    /// is ever admitted. Adopted from that first unit's epoch (mid-GOP
    /// join, or a v2 negotiated session's first nonzero epoch — both arrive
    /// with no preceding discontinuity, so `0` can't be assumed) rather than
    /// hardcoded, then bumped by every later discontinuity. Units/decoder
    /// output whose epoch doesn't match this predate the last reset and
    /// must never reach the present path or the recovery-ack call — the
    /// guard against stale output surviving a reset.
    epoch: Option<u32>,
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
            // Mid-GOP join (reconnect onto a resumed Sunshine session, or a
            // v2 negotiated session's first admitted unit): deltas keep
            // flowing and nothing else ever asks for a key, so without this
            // latch the session shows white forever (field report
            // 2026-07-15) — armed from the very first unit, whatever its
            // epoch turns out to be (see `admit_epoch`), same as before any
            // transport discontinuity has ever fired.
            wait_for_key: Some(0),
            recovery_generation: None,
            epoch: None,
        }
    }

    /// A `VideoEvent::Discontinuity` arrived: flush the FFmpeg reference
    /// chain (and any pending/drain output) and arm wait-for-key + the
    /// tracked recovery generation for the new epoch. Must be applied — in
    /// event order — before any later-queued frame is decoded, so a
    /// stale-epoch frame already in-flight in the backlog never reaches
    /// FFmpeg after the reset it predates.
    fn on_discontinuity(&mut self, generation: u64, epoch: u32) {
        self.epoch = Some(epoch);
        if let Some(dec) = self.decoder.as_mut() {
            dec.flush();
        }
        self.wait_for_key = Some(epoch);
        self.recovery_generation = Some(generation);
    }

    /// Adopts `epoch` as the working epoch on the very first admitted unit
    /// (mid-GOP join, or a v2 session's first nonzero epoch — both arrive
    /// with no preceding discontinuity), re-keying an already-armed
    /// `wait_for_key` to the adopted epoch so a real decoded key at that
    /// epoch can actually clear it (a mismatched hardcoded epoch would
    /// otherwise leave the gate latched forever). Returns `false` when
    /// `epoch` predates the last reset (backlog to skip), `true` otherwise.
    fn admit_epoch(&mut self, epoch: u32) -> bool {
        match self.epoch {
            None => {
                self.epoch = Some(epoch);
                if self.wait_for_key.is_some() {
                    self.wait_for_key = Some(epoch);
                }
                true
            }
            Some(current) => current == epoch,
        }
    }

    /// Pure decision, isolated from the RxCore/FFmpeg calls that act on
    /// it: does this decoder output clear wait-for-key recovery? Only a
    /// real decoded key (`meta.is_key`, derived from the AVFrame — never
    /// the submitted packet's own flag) matching the currently armed
    /// epoch qualifies. A failed key packet or a key packet that produced
    /// no output never reaches this call at all (there is no `DecodeMeta`
    /// to pass), so recovery stays latched by construction — this
    /// function only ever sees real decoded output.
    fn clears_recovery(armed: Option<u32>, meta: video::DecodeMeta) -> bool {
        armed == Some(meta.epoch) && meta.is_key
    }

    /// Apply a real decoded-output [`video::DecodeMeta`] to recovery
    /// state. When RxCore has an open recovery matching this instance's
    /// tracked `(recovery_generation, epoch)`, only its
    /// `acknowledge_decoded_key` succeeding clears the local latch —
    /// matching stale/wrong-generation double-checks on the RxCore side.
    /// When RxCore has no matching recovery open (the initial mid-GOP-join
    /// gate, before any transport discontinuity has ever fired), there is
    /// nothing to ack; the real decoded key still clears the local latch
    /// directly.
    fn on_meta(&mut self, core: &RxCore, meta: video::DecodeMeta) {
        if !Self::clears_recovery(self.wait_for_key, meta) {
            return;
        }
        match self.recovery_generation {
            Some(generation) if core.recovery() == Some((generation, meta.epoch)) => {
                if core.acknowledge_decoded_key(generation, meta.epoch, meta.frame_id) {
                    self.wait_for_key = None;
                    self.recovery_generation = None;
                }
            }
            _ => {
                self.wait_for_key = None;
                self.recovery_generation = None;
            }
        }
    }

    fn on_unit(
        &mut self,
        core: &RxCore,
        shared: &VideoShared,
        egui_ctx: &eframe::egui::Context,
        unit: &DecodeUnit,
    ) {
        if self.decoder.is_none() {
            return;
        }
        if !self.admit_epoch(unit.epoch) {
            // Backlog left over from before the last reset — never
            // decode/present it.
            return;
        }
        if self.wait_for_key.is_some() && !unit.is_key {
            // Deltas while armed are dropped and request a key, never fed
            // to the decoder.
            core.request_idr();
            return;
        }
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
        let dec = self.decoder.as_mut().expect("checked above");
        let decoded = if want_nv12 {
            dec.decode_nv12(unit.epoch, unit.frame_id, &unit.data)
                .map(|o| o.map(|d| (d.frame, d.meta)))
        } else {
            dec.decode(unit.epoch, unit.frame_id, &unit.data)
                .map(|o| o.map(|d| (video::DecodedFrame::Rgba(d.frame), d.meta)))
        };
        match decoded {
            Ok(Some((frame, meta))) if Some(meta.epoch) == self.epoch => {
                // G004 decoded-output heartbeat: fed exactly here, at the
                // point FFmpeg actually produced a picture — never on
                // packet submission (the `unit` this call received), so a
                // stuck decoder that keeps accepting units but stops
                // emitting pictures cannot fake liveness.
                core.note_decoded_output();
                self.on_meta(core, meta);
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
            // meta.epoch always equals unit.epoch (Decoder attaches the
            // input unit's epoch verbatim, never derived from decoder
            // state), and unit.epoch was already gated by `admit_epoch`
            // above in this same synchronous call — this arm is defensive
            // redundancy against that invariant changing, not a live race
            // the single-threaded pump can actually hit.
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => {
                shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                self.wait_for_key = self.epoch;
                core.request_idr();
                tracing::warn!(err = %e, frame_id = unit.frame_id, "decode failed — requesting IDR");
            }
        }
    }

    /// Advance the decoder over a stale unit without presenting it (skip
    /// backlog): same epoch/IDR gating as [`Self::on_unit`], but the
    /// picture is decoded and dropped instead of converted + published —
    /// still reports its [`video::DecodeMeta`] so a real decoded key
    /// hiding in the backlog still clears recovery.
    fn drop_unit(&mut self, core: &RxCore, shared: &VideoShared, unit: &DecodeUnit) {
        if self.decoder.is_none() {
            return;
        }
        if !self.admit_epoch(unit.epoch) {
            return;
        }
        if self.wait_for_key.is_some() && !unit.is_key {
            core.request_idr();
            return;
        }
        let dec = self.decoder.as_mut().expect("checked above");
        match dec.decode_drop(unit.epoch, unit.frame_id, &unit.data) {
            // Same defensive-redundancy note as `on_unit`: meta.epoch is
            // always unit.epoch, already gated above.
            Ok(Some(meta)) if Some(meta.epoch) == self.epoch => {
                // Same heartbeat contract as `on_unit`: only a real
                // decoded picture (backlog-drain path still decodes,
                // never presents) feeds it.
                core.note_decoded_output();
                self.on_meta(core, meta);
            }
            Ok(_) => {}
            Err(e) => {
                shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                self.wait_for_key = self.epoch;
                core.request_idr();
                tracing::warn!(err = %e, frame_id = unit.frame_id, "decode(drop) failed — requesting IDR");
            }
        }
    }

    /// G004 `DecodeStall` consumption: the watchdog decided the decode
    /// stage has stopped producing pictures despite units still flowing
    /// (native decode wedged, or the reference chain corrupted). Reuses
    /// the exact G002 flush+arm machinery `on_discontinuity` uses — flush
    /// the FFmpeg reference chain and arm `wait_for_key` — but keyed off
    /// the decode-side stall signal rather than a transport reset, so it
    /// never bumps `recovery_generation` (there is no RxCore recovery to
    /// open/ack for a purely decode-local stall) and requests an IDR
    /// directly instead of waiting for the next delta packet to trigger
    /// one via the existing `wait_for_key` gate in `on_unit`/`drop_unit`.
    fn on_decode_stall(&mut self, core: &RxCore) {
        if let Some(dec) = self.decoder.as_mut() {
            dec.flush();
        }
        self.wait_for_key = self.epoch;
        core.request_idr();
    }
}

/// A step [`drain_newest`] fires while draining the backlog: either the
/// unit that was `newest` before a later frame superseded it (to be
/// decoded-and-dropped), or a discontinuity's `(generation, epoch)` (to be
/// flushed/armed). One `on_event` closure carries both so callers needing a
/// single `&mut` capture (the production pump, which mutates the same
/// `DecodeState` either way) don't need two simultaneous closures over
/// it.
#[cfg(feature = "video")]
enum DrainStep<'a> {
    Dropped(&'a DecodeUnit),
    /// A pre-reset unit abandoned by a poisoning reset: accounting-only —
    /// it must never be decoded (unlike [`DrainStep::Dropped`]).
    Poisoned(&'a DecodeUnit),
    Reset(u64, u32),
}

/// Pure backlog-drain ordering used by the frame pump: given the just
/// dequeued `first` frame and a pull-more-events closure, drains strictly
/// in arrival order. A `Discontinuity` found mid-drain fires
/// `on_event(DrainStep::Reset(generation, epoch))` (flush + re-arm)
/// *before* any later frame is considered — the "process reset before later
/// retained frame" contract. Critically, whatever was `newest` at that
/// point is *poisoned*: it predates the reset and must never reach
/// `on_event(DrainStep::Dropped(_))` (decode-and-drop) afterwards, even when
/// the reset shares its epoch (most discontinuity reasons other than
/// `EpochTransition` do) — decoding it now could otherwise let a pre-reset
/// key wrongly ack the post-reset recovery generation, since the epoch gate
/// alone can't tell the two apart. Poisoned units surface as
/// `on_event(DrainStep::Poisoned(_))` (accounting-only, never decoded); the
/// next frame after a reset becomes the new baseline outright. When the
/// reset is the *last* drained event, no post-reset frame exists yet, so
/// the function returns `None` and the caller decodes nothing this tick —
/// the retained/next frame arrives as its own queue event. Free of
/// FFmpeg/RxCore I/O — only closures — so the ordering is provable in a
/// unit test without a live queue.
#[cfg(feature = "video")]
fn drain_newest(
    first: DecodeUnit,
    mut next_event: impl FnMut() -> Option<VideoEvent>,
    mut on_event: impl FnMut(DrainStep<'_>),
) -> Option<DecodeUnit> {
    let mut newest = first;
    let mut poisoned = false;
    loop {
        match next_event() {
            Some(VideoEvent::Frame(next)) => {
                if poisoned {
                    on_event(DrainStep::Poisoned(&newest));
                    poisoned = false;
                } else {
                    on_event(DrainStep::Dropped(&newest));
                }
                newest = next;
            }
            Some(VideoEvent::Discontinuity {
                generation, epoch, ..
            }) => {
                on_event(DrainStep::Reset(generation, epoch));
                poisoned = true;
            }
            None => break,
        }
    }
    if poisoned {
        on_event(DrainStep::Poisoned(&newest));
        None
    } else {
        Some(newest)
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
                        // G004 `DecodeStall` consumption: cross-thread
                        // signal from the UI-thread watchdog readback (see
                        // `VideoShared::decode_stall_pending`) — checked
                        // every iteration, so at least every
                        // `wait_event` timeout (250 ms) even with no unit
                        // flowing.
                        #[cfg(feature = "video")]
                        if video.decode_stall_pending.swap(false, Ordering::AcqRel) {
                            decode.on_decode_stall(&core);
                        }
                        match core.wait_event(Duration::from_millis(250)) {
                            Some(VideoEvent::Discontinuity {
                                generation, epoch, ..
                            }) => {
                                #[cfg(feature = "video")]
                                decode.on_discontinuity(generation, epoch);
                                #[cfg(not(feature = "video"))]
                                let _ = (generation, epoch);
                            }
                            Some(VideoEvent::Frame(unit)) => {
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
                                    // Skip stale backlog: decode older queued
                                    // units to keep the reference chain current
                                    // but only convert + present the newest, so
                                    // presentation latency never accumulates when
                                    // download/convert briefly falls behind 60 fps.
                                    // Ordered: a Discontinuity found mid-drain
                                    // flushes+arms immediately, before any later
                                    // frame in the backlog is considered — never
                                    // skipped/coalesced across the reset.
                                    // `account` runs exactly once per frame: in
                                    // `Dropped`/`Poisoned` for every superseded
                                    // or abandoned unit, and once more here for
                                    // whichever unit survives as `newest` —
                                    // accounting it up front instead would
                                    // double-count it if it later got superseded
                                    // while the frame that actually presents
                                    // would never be accounted at all.
                                    let newest = drain_newest(
                                        unit,
                                        || core.try_event(),
                                        |step| match step {
                                            DrainStep::Reset(generation, epoch) => {
                                                decode.on_discontinuity(generation, epoch);
                                            }
                                            DrainStep::Dropped(u) => {
                                                account(u);
                                                decode.drop_unit(&core, &video, u);
                                            }
                                            // Pre-reset unit killed by a reset:
                                            // accounted but never decoded.
                                            DrainStep::Poisoned(u) => account(u),
                                        },
                                    );
                                    if let Some(newest) = newest {
                                        stats.last_frame_ms.store(
                                            now.duration_since(started).as_millis() as u64,
                                            Ordering::Relaxed,
                                        );
                                        fps.push(now);
                                        account(&newest);
                                        decode.on_unit(&core, &video, &egui_ctx, &newest);
                                    }
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
    /// G006 fix: background host-boot in progress. `App::new`, the role
    /// switch, and "Restart host" spawn `host::start` on a worker thread
    /// (`App::start_host`) instead of blocking the UI thread for up to
    /// 20s (see `host::start`/supervisor ready-wait docs); each frame
    /// polls this receiver non-blockingly (`App::poll_host_starting`) and
    /// applies the result once it lands.
    host_starting: Option<mpsc::Receiver<Result<host::Host, String>>>,
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
        let mut app = Self {
            host_id_text: form.host_id.to_string(),
            app_id_text: form.app_id.to_string(),
            form,
            settings,
            running: None,
            host: None,
            host_error: None,
            host_starting: None,
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
        };
        // G006 fix: role=host/both makes the host role automatic/always-on
        // — boot it here instead of waiting for a manual click. The boot
        // itself runs on a background thread (`App::start_host` spawns
        // it), so this never blocks the client shell's first frame; a
        // boot failure surfaces through the existing `host_error` banner
        // once the worker thread's result lands (`poll_host_starting`),
        // exactly like a manual start failure.
        if host::role_wants_host(app.settings.role) {
            app.start_host();
        }
        app
    }

    /// Start the host role on a background thread and stash the result
    /// receiver in `host_starting` — the one seam both the automatic
    /// role-driven boot ([`App::new`]) and the manual restart control
    /// ([`App::host_section`]) call through, so they can never drift.
    /// `host::start` can block for up to ~20s (Foundation ready-wait), so
    /// this must never run on the UI thread; [`App::poll_host_starting`]
    /// (called every frame) is what actually applies the outcome to
    /// `self.host`/`self.host_error`.
    fn start_host(&mut self) {
        if self.host_starting.is_some() {
            // A boot is already in flight. Replacing the receiver would
            // orphan the first boot's Host once it lands (bound port plus
            // server/monitor threads with nothing left to stop them) and
            // the second boot would then fail its port bind. The pending
            // delivery is applied — or stopped, if the role has flipped
            // away meanwhile — by `poll_host_starting`.
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.host_starting = Some(rx);
        let settings = self.settings.clone();
        let spawned = std::thread::Builder::new()
            .name("bp-host-boot".into())
            .spawn(move || {
                let result = host::start(std::path::Path::new(host::DEFAULT_CONFIG_PATH), &settings);
                let _ = tx.send(result);
            });
        if let Err(e) = spawned {
            tracing::error!(err = %e, "failed to spawn host boot thread");
            self.host_error = Some(format!("failed to spawn host boot thread: {e}"));
            self.host_starting = None;
        }
    }

    /// Non-blocking poll of an in-flight background host boot (if any),
    /// applying the result to `self.host`/`self.host_error` exactly once
    /// it lands — called every frame from [`eframe::App::update`].
    fn poll_host_starting(&mut self) {
        let Some(rx) = &self.host_starting else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(h)) => {
                self.host_starting = None;
                let install = host::role_wants_host(self.settings.role) && self.host.is_none();
                if install {
                    self.host_error = None;
                }
                apply_host_delivery(install, h, &mut self.host, host::Host::stop);
            }
            Ok(Err(e)) => {
                tracing::error!(err = %e, "host role start failed");
                self.host_error = Some(e);
                self.host_starting = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                tracing::error!("host boot thread vanished without a result");
                self.host_error = Some("host boot thread vanished without a result".into());
                self.host_starting = None;
            }
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

    /// Role selector (G006): client/host/both, persisted to
    /// `settings.role`. Selecting host/both is what makes the host role
    /// automatic — the actual boot/stop happens via
    /// [`host::role_transition`]'s decision, applied right after the
    /// combo box, never touching `self.running` (the client session).
    fn role_label(role: settings::Role) -> &'static str {
        match role {
            settings::Role::Client => "Client",
            settings::Role::Host => "Host",
            settings::Role::Both => "Both",
        }
    }

    /// Host role strip (D1) + G006 status panel. Rendered on every
    /// screen — hosting and a client session may run together (role
    /// "both" is the LAN-party topology); the role switch never reaches
    /// into `self.running`.
    fn host_section(&mut self, ui: &mut eframe::egui::Ui) {
        use eframe::egui;

        let old_role = self.settings.role;
        let mut new_role = old_role;
        ui.horizontal(|ui| {
            ui.label("Role");
            egui::ComboBox::from_id_salt("role-select")
                .selected_text(Self::role_label(new_role))
                .show_ui(ui, |ui| {
                    for role in [settings::Role::Client, settings::Role::Host, settings::Role::Both]
                    {
                        ui.selectable_value(&mut new_role, role, Self::role_label(role));
                    }
                });
        });
        if new_role != old_role {
            self.settings.role = new_role;
            if let Err(e) = settings::save(&self.settings) {
                tracing::warn!(err = %e, "settings store save failed");
                self.host_error = Some(format!("settings store save failed: {e}"));
            }
            apply_role_action(
                host::role_transition(old_role, new_role),
                self,
                |app| app.start_host(),
                |app| {
                    if let Some(h) = app.host.take() {
                        h.stop();
                    }
                },
            );
        }

        let wants_host = host::role_wants_host(self.settings.role);
        let mut restart_clicked = false;
        match &self.host {
            None => {
                if self.host_starting.is_some() {
                    if wants_host {
                        ui.small("host starting…");
                    } else {
                        // Role flipped away mid-boot; the delivery will be
                        // stopped by poll_host_starting when it lands.
                        ui.small("winding down a pending host boot…");
                    }
                } else if wants_host {
                    // Automatic boot failed or hasn't run yet this tick
                    // — the manual restart control the spec asks for.
                    restart_clicked = ui.button("Retry host start").clicked();
                } else {
                    ui.small("host role off (switch to Host or Both to enable)");
                }
            }
            Some(h) => {
                restart_clicked = ui
                    .add_enabled(self.host_starting.is_none(), egui::Button::new("Restart host"))
                    .clicked();
                let snapshot = h.foundation_health();
                ui.label(format!(
                    "hosting on {} ({}) — sunshine: {} (restarts: {})",
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
                    host::health_state_label(snapshot.state),
                    snapshot.restarts_in_window,
                ));
                if let Some(pid) = snapshot.foundation_pid {
                    ui.small(format!("sunshine pid {pid}"));
                }
                if let Some(addr) = h.server.addrs().first() {
                    ui.small(format!("pairing: {}", host::pairing_hint(*addr, h.https)));
                }
                ui.small(format!(
                    "logs: {}",
                    self.settings.host.paths.logs_dir.display()
                ));
                let update_status = update::cached_status(
                    &update::staged_dir(&self.settings.host.paths.updates_dir),
                    &update::manifest_path(&self.settings.host.paths.updates_dir),
                );
                if matches!(update_status, update::UpdateStatus::Checking) {
                    // A background verify is in flight; keep repainting so
                    // the settled result appears without input events.
                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                }
                ui.small(format!("update: {}", update_status.status_line()));
                let session_events = h.recent_streamer_events();
                if !session_events.is_empty() {
                    ui.small("recent sessions:");
                    for event in session_events.iter().rev().take(5) {
                        ui.small(host::format_lifecycle_event(event));
                    }
                }
                if let Some(e) = &snapshot.last_error {
                    ui.colored_label(eframe::egui::Color32::YELLOW, format!("sunshine: {e}"));
                }
            }
        }
        if restart_clicked {
            if let Some(h) = self.host.take() {
                h.stop();
            }
            self.start_host();
        }
        if let Some(e) = &self.host_error {
            ui.colored_label(eframe::egui::Color32::RED, format!("host: {e}"));
        }
    }
}
/// G006 finding-2 fix: the one seam that turns a [`host::RoleAction`]
/// into a mutation on whatever "host slot" a caller has — the production
/// `App::host_section` role switch (`slot` = `&mut App`, mutating
/// `App::host`/`App::host_starting` through `start_host`) and the
/// `g006_role_tests::RoleHarness` isolation harness (`slot` = a bare
/// `bool` stand-in) both call through this exact dispatch, so they
/// cannot drift apart. `start`/`stop` are `FnOnce(&mut T)` rather than
/// closures over `self`, precisely so two closures can be constructed at
/// the call site without both trying to hold `self` mutably at once —
/// only the one the `action` selects ever actually runs.
fn apply_role_action<T>(
    action: host::RoleAction,
    slot: &mut T,
    start: impl FnOnce(&mut T),
    stop: impl FnOnce(&mut T),
) {
    match action {
        host::RoleAction::StartHost => start(slot),
        host::RoleAction::StopHost => stop(slot),
        host::RoleAction::NoChange => {}
    }
}

/// Applies a background host-boot delivery ([`App::poll_host_starting`]):
/// the delivered instance is installed only when `install` is true (the
/// current role still wants a host AND the slot is empty); otherwise it is
/// stopped immediately, so a role flip during the boot window can never
/// leave a host running under a client-only role and an occupied slot is
/// never silently replaced (the late duplicate is torn down instead).
fn apply_host_delivery<T>(
    install: bool,
    delivered: T,
    slot: &mut Option<T>,
    stop: impl FnOnce(T),
) {
    if install {
        *slot = Some(delivered);
    } else {
        stop(delivered);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        use eframe::egui;

        #[cfg(not(all(windows, feature = "video")))]
        let _ = &frame;

        self.poll_host_starting();

        // Live counters need continuous repaint while connected; a
        // pending host boot also needs it so the "host starting…" panel
        // notices the background thread's result promptly.
        if self.running.is_some() || self.host_starting.is_some() {
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
                    // G004: propagate the typed decode/present stall
                    // rungs to the threads that own the resources able to
                    // act on them (the pump thread owns the live
                    // `Decoder`, the raw present thread owns the D3D11
                    // device) — the UI thread only reads the watchdog and
                    // relays it, edge-triggered, via `VideoShared`.
                    #[cfg(feature = "video")]
                    if run.session.watchdog().poll_decode_stall().is_some() {
                        run.video
                            .decode_stall_pending
                            .store(true, Ordering::Release);
                    }
                    #[cfg(all(windows, feature = "video"))]
                    if run.session.watchdog().poll_present_stall().is_some() {
                        run.video
                            .present_stall_pending
                            .store(true, Ordering::Release);
                    }
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
                                            run.core.clone(),
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
                                            // auto-switch + foreground
                                            // guard: fullscreen and the
                                            // keyboard hook stay engaged
                                            // for the whole session; the
                                            // mouse relative flag + clip
                                            // follow the host, and ANY
                                            // foreign foreground window
                                            // (dead hook Alt+Tab escape,
                                            // parent-focus loss, UAC)
                                            // releases capture instead of
                                            // caging the cursor to a
                                            // background window.
                                            match immersive::frame_capture(
                                                s.is_foreground(),
                                                run.session.cursor().visible(),
                                            ) {
                                                immersive::FrameCapture::ReleaseAndExit => {
                                                    tracing::info!(
                                                        "immersive foreground lost — releasing capture and exiting"
                                                    );
                                                    self.capture.set_relative(false);
                                                    self.capture.set_keyboard_capture(false);
                                                    s.release_mouse_capture();
                                                    input::release_sticky_keys(
                                                        &run.session.input_sender(),
                                                    );
                                                    self.capture.request_exit();
                                                }
                                                immersive::FrameCapture::Clip => {
                                                    self.capture.set_relative(true);
                                                    s.clip_cursor_to_self();
                                                }
                                                immersive::FrameCapture::Unclip => {
                                                    self.capture.set_relative(false);
                                                    s.release_cursor_clip();
                                                }
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
                                    // G004 present-success heartbeat, egui
                                    // fallback path: fed exactly here, at
                                    // the actual paint call that puts
                                    // pixels on screen — never at texture
                                    // upload/conversion above, so a stuck
                                    // egui repaint (window occluded,
                                    // viewport not drawing) cannot fake
                                    // present liveness either.
                                    run.core.note_presented();
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

/// G002/G004 state-machine tests: pure/mockable, no FFmpeg I/O (RxCore
/// itself is pure in-memory state, exercised directly). Cover the
/// flush/reset, failed-or-no-output-key-never-clears, stale-epoch,
/// successful-key-ack and overflow-reset-before-frame contracts directly
/// against [`DecodeState`]'s decision functions and [`drain_newest`]'s
/// ordering, plus G004's `DecodeStall` consumption
/// ([`DecodeState::on_decode_stall`]) and the decoded-output heartbeat's
/// submission-cannot-fake-it guarantee.
#[cfg(all(test, feature = "video"))]
mod g002_recovery_tests {
    use super::*;

    fn meta(epoch: u32, frame_id: u32, is_key: bool) -> video::DecodeMeta {
        video::DecodeMeta {
            epoch,
            frame_id,
            is_key,
        }
    }

    fn unit(epoch: u32, frame_id: u32, is_key: bool) -> DecodeUnit {
        DecodeUnit {
            frame_id,
            epoch,
            is_key,
            timestamp_us: 0,
            duration_us: 0,
            data: Vec::new(),
        }
    }

    // ── successful key ack ──────────────────────────────────────────────

    #[test]
    fn clears_recovery_only_for_matching_epoch_real_key() {
        assert!(DecodeState::clears_recovery(Some(3), meta(3, 10, true)));
    }

    #[test]
    fn clears_recovery_false_when_not_armed() {
        assert!(!DecodeState::clears_recovery(None, meta(3, 10, true)));
    }

    // ── stale epoch output ──────────────────────────────────────────────

    #[test]
    fn clears_recovery_false_for_stale_epoch() {
        // Armed for epoch 3, but the decoded output is tagged epoch 2 (a
        // reset raced the decode) — must never clear recovery for the
        // *current* epoch based on stale-epoch output.
        assert!(!DecodeState::clears_recovery(Some(3), meta(2, 10, true)));
    }

    // ── failed / no-output key never clears recovery ────────────────────

    #[test]
    fn on_meta_clears_local_latch_without_ack_when_no_recovery_open() {
        // Fresh RxCore: recovery() is None (no Discontinuity ever fired) —
        // a real decoded key still clears the local mid-GOP-join latch even
        // though there is nothing for RxCore to ack.
        let core = RxCore::new(0);
        let mut st = DecodeState {
            decoder: None,
            wait_for_key: Some(0),
            recovery_generation: None,
            epoch: Some(0),
        };
        st.on_meta(&core, meta(0, 42, true));
        assert_eq!(st.wait_for_key, None);
    }

    #[test]
    fn on_meta_never_clears_latch_for_non_key_output() {
        // `Decoder::decode`/`decode_drop` return `Ok(None)`/`Err` for a
        // failed or output-less key packet — there is no `DecodeMeta` to
        // construct in that case, so `on_meta` is never even called; the
        // armed state is untouched by construction. This test exercises
        // the adjacent case that *is* callable — a decoded delta output —
        // to prove `on_meta` itself never clears the latch for non-key
        // metadata either.
        let core = RxCore::new(0);
        let mut st = DecodeState {
            decoder: None,
            wait_for_key: Some(0),
            recovery_generation: None,
            epoch: Some(0),
        };
        st.on_meta(&core, meta(0, 42, false));
        assert_eq!(
            st.wait_for_key,
            Some(0),
            "non-key meta must never clear recovery"
        );
    }

    #[test]
    fn delta_is_key_false_never_clears_recovery_even_if_mislabelled() {
        // A packet claiming `is_key: false` can never clear recovery
        // regardless of epoch match — only real decoded-key metadata
        // (`meta.is_key`) does, never the submitted packet's own flag.
        assert!(!DecodeState::clears_recovery(Some(3), meta(3, 10, false)));
    }

    // ── flush/reset arms the new epoch + recovery generation ────────────

    #[test]
    fn discontinuity_arms_wait_for_key_for_new_epoch() {
        // `on_discontinuity` needs a live `Decoder`, which needs FFmpeg;
        // exercise the epoch/arm bookkeeping directly without one by
        // constructing the struct fields it touches.
        let mut st = DecodeState {
            decoder: None,
            wait_for_key: None,
            recovery_generation: None,
            epoch: Some(4),
        };
        st.on_discontinuity(11, 7);
        assert_eq!(st.epoch, Some(7));
        assert_eq!(st.wait_for_key, Some(7));
        assert_eq!(st.recovery_generation, Some(11));
    }

    // ── G004: DecodeStall consumption ────────────────────────────────────

    #[test]
    fn on_decode_stall_arms_wait_for_key_and_requests_idr_without_a_generation() {
        let core = RxCore::new(0);
        let mut st = DecodeState {
            decoder: None,
            wait_for_key: None,
            recovery_generation: Some(3), // pre-existing, must survive untouched
            epoch: Some(7),
        };
        st.on_decode_stall(&core);
        assert_eq!(
            st.wait_for_key,
            Some(7),
            "armed for the current epoch, same shape as on_discontinuity's arm"
        );
        assert_eq!(
            st.recovery_generation,
            Some(3),
            "a decode-local stall never opens/bumps an RxCore recovery generation \
             — unlike on_discontinuity, there is no transport reset to track"
        );
        assert!(
            core.poll_needs_idr(),
            "must request an IDR directly rather than waiting for the next delta"
        );
    }

    #[test]
    fn on_decode_stall_arms_none_when_no_epoch_seen_yet() {
        // Mirrors `on_meta`'s "nothing to ack" branch: before the first
        // unit is ever admitted, `epoch` is `None` — arming stays `None`
        // too (there is no epoch yet for a later real key to match), but
        // the IDR request still fires so the stalled stream unwedges as
        // soon as a key arrives.
        let core = RxCore::new(0);
        let mut st = DecodeState {
            decoder: None,
            wait_for_key: None,
            recovery_generation: None,
            epoch: None,
        };
        st.on_decode_stall(&core);
        assert_eq!(st.wait_for_key, None);
        assert!(core.poll_needs_idr());
    }

    // ── G004: decoded-output heartbeat cannot be faked by submission ────

    #[test]
    fn on_unit_never_feeds_decoded_heartbeat_without_reaching_the_decoder() {
        // No live `Decoder` at all: `on_unit` returns before it could
        // ever observe real FFmpeg output. Submitting a unit alone must
        // never bump the heartbeat counter.
        let core = RxCore::new(0);
        let shared = VideoShared::default();
        let egui_ctx = eframe::egui::Context::default();
        let mut st = DecodeState {
            decoder: None,
            wait_for_key: None,
            recovery_generation: None,
            epoch: Some(0),
        };
        st.on_unit(&core, &shared, &egui_ctx, &unit(0, 1, true));
        assert_eq!(core.decoded_output_count(), 0);
    }

    #[test]
    fn on_unit_never_feeds_decoded_heartbeat_for_a_delta_gated_while_armed() {
        // A delta packet while `wait_for_key` is armed is dropped and an
        // IDR requested *before the decoder ever runs* (queueing/
        // submission only) — the heartbeat must stay at zero. A live
        // `Decoder` (FFmpeg links unconditionally, no `FFMPEG_DIR`
        // needed — same as `video.rs`'s `decoder_initializes_hw_or_sw`)
        // so this actually exercises the gate ahead of `decode()`,
        // instead of short-circuiting on the "no decoder at all" case
        // already covered by the sibling test above.
        let core = RxCore::new(0);
        let shared = VideoShared::default();
        let egui_ctx = eframe::egui::Context::default();
        let mut st = DecodeState {
            decoder: Some(video::Decoder::new().expect("decoder init")),
            wait_for_key: Some(0),
            recovery_generation: None,
            epoch: Some(0),
        };
        st.on_unit(&core, &shared, &egui_ctx, &unit(0, 2, false));
        assert_eq!(core.decoded_output_count(), 0);
        assert!(
            core.poll_needs_idr(),
            "still requests a key on the gated delta"
        );
    }

    #[test]
    fn drop_unit_never_feeds_decoded_heartbeat_for_stale_epoch_backlog() {
        // `admit_epoch` rejects backlog predating the last reset before
        // the decoder is touched — never-decoded backlog must never feed
        // the heartbeat either.
        let core = RxCore::new(0);
        let shared = VideoShared::default();
        let mut st = DecodeState {
            decoder: None,
            wait_for_key: None,
            recovery_generation: None,
            epoch: Some(5),
        };
        st.drop_unit(&core, &shared, &unit(4, 9, true));
        assert_eq!(core.decoded_output_count(), 0);
    }

    // ── B1: adoptive first epoch (mid-GOP join / v2 nonzero first epoch) ──

    #[test]
    fn admit_epoch_adopts_first_unit_epoch_instead_of_assuming_zero() {
        // A v2 negotiated session's first admitted unit can carry any
        // nonzero epoch with no preceding discontinuity (VideoReceiver's
        // own `active_epoch` starts unset and silently accepts the first
        // epoch it sees — see `accept_epoch` in transport-core). DecodeState
        // must adopt it, not assume 0, or every such unit is silently
        // dropped forever by the stale-epoch guard in `on_unit`/`drop_unit`.
        let mut st = DecodeState {
            decoder: None,
            wait_for_key: Some(0),
            recovery_generation: None,
            epoch: None,
        };
        assert!(st.admit_epoch(7), "first-ever unit is always admitted");
        assert_eq!(st.epoch, Some(7));
        assert_eq!(
            st.wait_for_key,
            Some(7),
            "the mid-GOP-join gate must re-key to the adopted epoch, not stay latched at 0"
        );
        // A later unit at the same (adopted) epoch is admitted; one at a
        // different epoch (backlog predating a reset not yet observed) is
        // not.
        assert!(st.admit_epoch(7));
        assert!(!st.admit_epoch(8));
    }

    // ── B2: same-epoch reset poisons the pre-reset unit ──────────────────

    #[test]
    fn drain_newest_processes_reset_before_later_retained_frame() {
        let mut order: Vec<String> = Vec::new();
        let first = unit(0, 1, false);
        let f2 = unit(0, 2, false);
        let f3 = unit(1, 3, false);
        let mut events: std::collections::VecDeque<VideoEvent> = [
            VideoEvent::Frame(f2),
            VideoEvent::Discontinuity {
                generation: 1,
                epoch: 1,
                reason: transport_core::video_rx::DiscontinuityReason::EpochTransition,
            },
            VideoEvent::Frame(f3),
        ]
        .into();
        let result = drain_newest(
            first,
            || events.pop_front(),
            |step| match step {
                DrainStep::Reset(generation, epoch) => {
                    order.push(format!("reset:{generation}:{epoch}"))
                }
                DrainStep::Dropped(u) => order.push(format!("drop:{}", u.frame_id)),
                DrainStep::Poisoned(u) => order.push(format!("poison:{}", u.frame_id)),
            },
        );
        assert_eq!(
            order,
            vec![
                "drop:1".to_string(),
                "reset:1:1".to_string(),
                "poison:2".to_string(),
            ],
            "unit 2 (newest when the reset arrived) is poisoned — accounted but never Dropped"
        );
        let result = result.expect("a post-reset frame survives");
        assert_eq!(result.frame_id, 3);
        assert_eq!(result.epoch, 1);
    }

    #[test]
    fn drain_newest_poisons_the_pre_reset_unit_across_a_same_epoch_reset() {
        // The aliasing case: a same-epoch reset (e.g. QueueOverflow —
        // production never bumps the epoch for it) must still poison
        // whatever was `newest` at that point. The epoch gate alone cannot
        // tell a same-epoch pre-reset unit apart from a legitimate
        // post-reset one, so without poisoning, unit 2 would wrongly reach
        // `Dropped` (decode-and-drop) after the reset and could ack the
        // post-reset recovery generation with a pre-reset key.
        let mut order: Vec<String> = Vec::new();
        let first = unit(0, 1, false);
        let f2 = unit(0, 2, false);
        let f3 = unit(0, 3, false); // same epoch as both `first` and the reset
        let mut events: std::collections::VecDeque<VideoEvent> = [
            VideoEvent::Frame(f2),
            VideoEvent::Discontinuity {
                generation: 5,
                epoch: 0,
                reason: transport_core::video_rx::DiscontinuityReason::QueueOverflow,
            },
            VideoEvent::Frame(f3),
        ]
        .into();
        let result = drain_newest(
            first,
            || events.pop_front(),
            |step| match step {
                DrainStep::Reset(generation, epoch) => {
                    order.push(format!("reset:{generation}:{epoch}"))
                }
                DrainStep::Dropped(u) => order.push(format!("drop:{}", u.frame_id)),
                DrainStep::Poisoned(u) => order.push(format!("poison:{}", u.frame_id)),
            },
        );
        assert_eq!(
            order,
            vec![
                "drop:1".to_string(),
                "reset:5:0".to_string(),
                "poison:2".to_string(),
            ],
            "unit 2 must never be Dropped even though it shares the reset's epoch"
        );
        let result = result.expect("a post-reset frame survives");
        assert_eq!(
            result.frame_id, 3,
            "unit 3 becomes the new baseline outright, not via Dropped-supersede"
        );
        assert_eq!(
            result.epoch, 0,
            "same epoch as the reset — the aliasing case"
        );
    }

    #[test]
    fn drain_newest_trailing_reset_abandons_the_pre_reset_unit_and_returns_none() {
        // A transport-sourced same-epoch discontinuity can be the *last*
        // drained event (push_discontinuity is a standalone push, unlike
        // overflow's atomic reset+frame pair). The pre-reset unit must not
        // survive as the returned baseline: it would decode post-flush and
        // could ack the fresh recovery generation with a pre-reset key.
        let mut order: Vec<String> = Vec::new();
        let first = unit(0, 1, true);
        let mut events: std::collections::VecDeque<VideoEvent> = [VideoEvent::Discontinuity {
            generation: 9,
            epoch: 0,
            reason: transport_core::video_rx::DiscontinuityReason::ReorderGap,
        }]
        .into();
        let result = drain_newest(
            first,
            || events.pop_front(),
            |step| match step {
                DrainStep::Reset(generation, epoch) => {
                    order.push(format!("reset:{generation}:{epoch}"))
                }
                DrainStep::Dropped(u) => order.push(format!("drop:{}", u.frame_id)),
                DrainStep::Poisoned(u) => order.push(format!("poison:{}", u.frame_id)),
            },
        );
        assert_eq!(
            order,
            vec!["reset:9:0".to_string(), "poison:1".to_string()],
            "the pre-reset key is accounted via Poisoned, never Dropped/decoded"
        );
        assert!(
            result.is_none(),
            "no frame survives a trailing reset — nothing decodes this tick"
        );
    }

    #[test]
    fn drain_newest_no_events_returns_first_unchanged() {
        let first = unit(2, 9, true);
        let out = drain_newest(first.clone(), || None, |_| {});
        assert_eq!(out, Some(first));
    }
}

/// G006 §3/§4: role-transition isolation + pure decision-logic tests.
/// Unconditional (no `video`/`windows` gate) — these exercise
/// `host::role_wants_host`/`role_transition` plus the App-level wiring
/// contract, none of which touch platform-specific rendering.
#[cfg(test)]
mod g006_role_tests {
    use super::*;
    use std::sync::Arc;

    /// The review-flagged race: a role flip back to Client while a
    /// background host boot is still in flight must stop the delivered
    /// host instead of installing it — and an occupied slot must never
    /// be silently replaced by a late duplicate delivery.
    #[test]
    fn a_host_delivered_after_flipping_back_to_client_is_stopped_not_installed() {
        use std::cell::Cell;

        // Role flipped to Client before the boot landed → not installed,
        // fully stopped.
        let stopped = Cell::new(false);
        let mut slot: Option<&str> = None;
        apply_host_delivery(false, "late-host", &mut slot, |_| stopped.set(true));
        assert!(slot.is_none());
        assert!(stopped.get());

        // Role still wants a host and the slot is empty → installed,
        // never stopped.
        let stopped = Cell::new(false);
        let mut slot: Option<&str> = None;
        apply_host_delivery(true, "wanted-host", &mut slot, |_| stopped.set(true));
        assert_eq!(slot, Some("wanted-host"));
        assert!(!stopped.get());

        // Slot already occupied → the late duplicate is torn down and the
        // existing host is untouched (poll passes install=false then).
        let stopped = Cell::new(false);
        let mut slot = Some("existing");
        apply_host_delivery(false, "duplicate", &mut slot, |_| stopped.set(true));
        assert_eq!(slot, Some("existing"));
        assert!(stopped.get());
    }
    /// Minimal role-driven host lifecycle harness mirroring exactly what
    /// `App::host_section`'s role-switch wiring does to `App::host` —
    /// without spinning a real `host::Host`. `host_up` stands in for
    /// `App::host: Option<host::Host>` (Some ⇔ host role running);
    /// `client_session` stands in for `App::running: Option<Running>`,
    /// using an `Arc` so the test can prove the *same* handle survives
    /// every role change (a real `Running` drop tears down the session's
    /// threads/sockets — swapping or dropping the pointer here would be
    /// the exact bug G006 §3 forbids).
    struct RoleHarness {
        role: settings::Role,
        host_up: bool,
        client_session: Option<Arc<()>>,
    }

    impl RoleHarness {
        /// Applies a role change through the exact same
        /// [`apply_role_action`] seam `App::host_section` calls —
        /// mutating only `host_up`. Sharing the seam (rather than
        /// re-deriving the match here) is what prevents this test from
        /// drifting from production wiring; it does not by itself prove
        /// isolation beyond "this dispatch has no `client_session`
        /// parameter to touch".
        fn apply_role(&mut self, new_role: settings::Role) {
            apply_role_action(
                host::role_transition(self.role, new_role),
                &mut self.host_up,
                |up| *up = true,
                |up| *up = false,
            );
            self.role = new_role;
        }
    }

    #[test]
    fn role_transitions_never_disturb_an_active_client_session() {
        let session = Arc::new(());
        let mut h = RoleHarness {
            role: settings::Role::Client,
            host_up: false,
            client_session: Some(session.clone()),
        };
        assert!(!h.host_up);

        h.apply_role(settings::Role::Both);
        assert!(h.host_up, "client->both must boot the host role");
        assert!(
            h.client_session
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, &session)),
            "client session handle must survive client->both"
        );

        h.apply_role(settings::Role::Host);
        assert!(
            h.host_up,
            "both->host keeps the host running (still host-wanting)"
        );
        assert!(
            h.client_session
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, &session)),
            "client session handle must survive both->host"
        );

        h.apply_role(settings::Role::Client);
        assert!(!h.host_up, "host->client must stop the host role");
        assert!(
            h.client_session
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, &session)),
            "client session handle must survive host->client"
        );
        assert_eq!(
            Arc::strong_count(&session),
            2,
            "no extra clone or drop of the session handle across the whole cycle \
             (one strong ref in `session`, one in `client_session`)"
        );
    }

    #[test]
    fn role_label_covers_every_role() {
        assert_eq!(App::role_label(settings::Role::Client), "Client");
        assert_eq!(App::role_label(settings::Role::Host), "Host");
        assert_eq!(App::role_label(settings::Role::Both), "Both");
    }
}
