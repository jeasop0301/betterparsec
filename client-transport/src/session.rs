//! Async client session: login → WS signaling → WebRTC peer (answerer) →
//! `video_fec` subscribe → [`RxCore`] feed (m6-native-spike.md §F).
//!
//! Threading: [`Session::start`] spawns a dedicated tokio runtime thread;
//! the caller (C ABI or ct-probe) pulls frames through the shared
//! [`RxCore`] and stops via [`Session::stop`].

use std::future::ready;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use common::api_bindings::{
    RtcIceCandidate, RtcIceServer, RtcSdpType, RtcSessionDescription, StreamClientMessage,
    StreamServerMessage, StreamSignalingMessage,
};
use common::input_wire::InboundPacket;
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tracing::{debug, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::{API, APIBuilder};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::capi::{FecControl, RxCore, SessionGeneration};
use crate::cursor::{CursorShared, decode_pos, decode_shape};
use crate::flow::{FlowAction, FlowConfig, SignalingFlow, StreamParams, negotiated_channels};
use crate::tls::{ServerTrust, client_config};
use crate::watchdog::{WatchdogAction, WatchdogConfig, WatchdogStage, WatchdogSupervisor};

/// H.264 baseline bit for `FlowConfig::supported_codecs`
/// (mirrors StreamSupportedVideoCodecs::H264).
pub const H264_BIT: u32 = common::api_bindings::StreamSupportedVideoCodecs::H264;

// ── Public config / state ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// e.g. `https://localhost:8080` (no trailing slash, no `/api`).
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub trust: ServerTrust,
    pub flow: FlowConfig,
}

/// Coarse observable state for pollers (C ABI / probe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Connecting = 0,
    PeerConnected = 1,
    /// `ConnectionComplete` received — media is live.
    Streaming = 2,
    Failed = 3,
    Stopped = 4,
}

/// Structured incident snapshot (G004): captured whenever any watchdog
/// stage takes action and pollable over the C ABI
/// (`ct_session_poll_incident`) for host-side diagnostics/telemetry. POD —
/// safe to read from any thread; `_present` flags distinguish "field is
/// legitimately zero" from "field has never been observed".
#[derive(Debug, Clone, Copy, Default)]
pub struct IncidentSnapshot {
    /// Actual `SessionGeneration` lease value (see
    /// [`crate::capi::RxCore::acquire_session_lease`]) the incident was
    /// raised under — stable for the life of one `Session`; distinct
    /// across reconnects, since each `Session::try_start` acquires a fresh
    /// lease. Stamped into [`WatchdogSupervisor`] at `dog.start()`.
    pub generation: u64,
    /// Diagnostic frame id of the most recently queued frame.
    pub last_frame_id: u32,
    pub last_frame_id_present: bool,
    /// Currently negotiated FEC epoch, if selected.
    pub active_epoch: u32,
    pub active_epoch_present: bool,
    // Reassembly/FEC counters — `VideoReceiverStats` passthrough (already a
    // superset wrapping `FecDecoderStats`'s symbol counters).
    pub source_symbols_received: u64,
    pub repair_symbols_received: u64,
    pub symbols_recovered: u64,
    pub frames_recovered: u64,
    pub frames_dropped_awaiting_idr: u64,
    pub loss_spans: u64,
    pub loss_spans_recovered: u64,
    /// Frame-queue depth at incident time (bound/backlog counter).
    pub queue_len: u32,
    /// Diagnostic (non-consuming) copy of the latest observed ACK value.
    pub last_ack: u32,
    pub last_ack_present: bool,
    /// 1-based attempt of the most recent `RequestIdr` rung fired by the
    /// receive-stage ladder this episode (0 = none fired yet).
    pub last_idr_attempt: u32,
    /// Heartbeat ages (ms since last progress) for all three watchdog
    /// stages plus audio.
    pub receive_age_ms: u64,
    pub decode_age_ms: u64,
    pub present_age_ms: u64,
    pub audio_age_ms: u64,
    /// Recovery outcome: the open decoder-recovery generation/epoch, if
    /// any (see [`RxCore::recovery`]).
    pub recovery_open: bool,
    pub recovery_generation: u64,
    pub recovery_epoch: u32,
}

/// M4 stall-watchdog readback shared with the shell: indicator state +
/// terminal reconnect request (the shell tears the session down and
/// rebuilds — mirrors the web wiring's reconnect rung). Extended by G004
/// with typed decode/present stall polls and a pollable incident snapshot.
#[derive(Debug, Default)]
pub struct WatchdogStatus {
    stalled: AtomicBool,
    reconnect: AtomicBool,
    /// 1-based attempt of a pending `DecodeStall` action; 0 = none
    /// pending. Poll-and-clear via [`WatchdogStatus::poll_decode_stall`].
    decode_stall_attempt: AtomicU32,
    /// Same shape as `decode_stall_attempt`, for `PresentStall`.
    present_stall_attempt: AtomicU32,
    /// The real `SessionGeneration` lease generation each latch above was
    /// recorded under (G004) — `poll_decode_stall`/`poll_present_stall`
    /// compare this against `generation` and drop a latched attempt raised
    /// under a superseded generation instead of handing a stale action to
    /// the (genuinely asynchronous, cross-thread) native pump.
    decode_stall_generation: AtomicU64,
    present_stall_generation: AtomicU64,
    /// Current session generation, stamped once via
    /// [`WatchdogStatus::set_generation`] whenever `dog.start()` runs (see
    /// `run_session`'s `FlowAction::Complete` handling).
    generation: AtomicU64,
    /// Latest incident snapshot (G004); non-consuming peek via
    /// [`WatchdogStatus::latest_incident`].
    incident: std::sync::Mutex<Option<IncidentSnapshot>>,
}

impl WatchdogStatus {
    /// Stall indicator is up (UI readback).
    pub fn stalled(&self) -> bool {
        self.stalled.load(Ordering::Acquire)
    }

    /// The ladder exhausted into the terminal reconnect rung; the session
    /// thread has ended (state `Failed`) and the shell should rebuild.
    pub fn reconnect_requested(&self) -> bool {
        self.reconnect.load(Ordering::Acquire)
    }

    /// Record the session generation live as of `dog.start()` (G004) — see
    /// the struct-level doc for why `poll_decode_stall`/`poll_present_stall`
    /// need this.
    fn set_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
    }

    /// Poll-and-clear the pending decode-stall attempt, if any, dropping it
    /// if it was latched under a since-superseded generation (see the
    /// struct-level doc). The native pump maps a delivered attempt to
    /// decoder flush + IDR.
    pub fn poll_decode_stall(&self) -> Option<u32> {
        match self.decode_stall_attempt.swap(0, Ordering::AcqRel) {
            0 => None,
            attempt => {
                let recorded = self.decode_stall_generation.load(Ordering::Acquire);
                let current = self.generation.load(Ordering::Acquire);
                (recorded == current).then_some(attempt)
            }
        }
    }

    /// Poll-and-clear the pending present-stall attempt, if any, dropping it
    /// if it was latched under a since-superseded generation (see the
    /// struct-level doc). The native pump maps a delivered attempt to
    /// device recreate / R8 fallback (escalation bounded on the native
    /// side; a final reconnect rung is a plain `reconnect_requested`).
    pub fn poll_present_stall(&self) -> Option<u32> {
        match self.present_stall_attempt.swap(0, Ordering::AcqRel) {
            0 => None,
            attempt => {
                let recorded = self.present_stall_generation.load(Ordering::Acquire);
                let current = self.generation.load(Ordering::Acquire);
                (recorded == current).then_some(attempt)
            }
        }
    }

    /// Latest incident snapshot, if any watchdog stage has acted yet.
    /// Non-consuming — safe to poll repeatedly (e.g. from a UI/telemetry
    /// timer) without racing the event that produced it.
    pub fn latest_incident(&self) -> Option<IncidentSnapshot> {
        *self
            .incident
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Latch a pending `DecodeStall`/`PresentStall` attempt (stamped with
    /// `snapshot.generation`, the real session generation live at record
    /// time) and publish the incident snapshot — called once from the
    /// session loop whenever a watchdog stage acts (see `run_session`'s
    /// `dog.tick()` handling).
    fn record_action(
        &self,
        stage: WatchdogStage,
        action: WatchdogAction,
        snapshot: IncidentSnapshot,
    ) {
        match (stage, action) {
            (WatchdogStage::Decode, WatchdogAction::DecodeStall { attempt }) => {
                self.decode_stall_generation
                    .store(snapshot.generation, Ordering::Release);
                self.decode_stall_attempt.store(attempt, Ordering::Release);
            }
            (WatchdogStage::Present, WatchdogAction::PresentStall { attempt }) => {
                self.present_stall_generation
                    .store(snapshot.generation, Ordering::Release);
                self.present_stall_attempt.store(attempt, Ordering::Release);
            }
            _ => {}
        }
        *self
            .incident
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
    }
}

pub struct Session {
    state: Arc<AtomicU8>,
    params: Arc<std::sync::Mutex<Option<StreamParams>>>,
    stop_tx: mpsc::Sender<()>,
    input_tx: mpsc::Sender<(u8, Vec<u8>)>,
    watchdog: Arc<WatchdogStatus>,
    watchdog_paused: Arc<AtomicBool>,
    cursor: Arc<CursorShared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Spawn the session on its own runtime thread. Frames/ACKs/needs-IDR
    /// flow through `core`; the caller keeps pulling from it as usual.
    ///
    /// This compatibility wrapper reports a rejected lease as a failed session.
    /// C callers that must not receive a handle use [`Session::try_start`].
    pub fn start(config: SessionConfig, core: Arc<RxCore>) -> Self {
        match Self::try_start(config, core) {
            Ok(session) => session,
            Err(error) => {
                warn!(?error, "session start rejected: receiver is already owned");
                Self::rejected()
            }
        }
    }

    /// Atomically acquire the receiver's one-shot lease and start its session.
    /// A rejected lease returns an error without creating a session handle.
    pub fn try_start(config: SessionConfig, core: Arc<RxCore>) -> Result<Self, &'static str> {
        let state = Arc::new(AtomicU8::new(SessionState::Connecting as u8));
        let params = Arc::new(std::sync::Mutex::new(None));
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        // Input backlog ~= a few frames of events; overflow drops (stale
        // input is worse than lost input).
        let (input_tx, input_rx) = mpsc::channel::<(u8, Vec<u8>)>(512);
        let watchdog = Arc::new(WatchdogStatus::default());
        let watchdog_paused = Arc::new(AtomicBool::new(false));
        let cursor = Arc::new(CursorShared::default());

        let generation = core.acquire_session_lease()?;
        core.begin_fec_negotiation();
        let state2 = state.clone();
        let params2 = params.clone();
        let watchdog2 = watchdog.clone();
        let watchdog_paused2 = watchdog_paused.clone();
        let cursor2 = cursor.clone();
        let thread_core = core.clone();
        let generation_for_thread = generation;
        let thread = match std::thread::Builder::new()
            .name("ct-session".into())
            .spawn(move || {
                let mut cleanup = SessionCleanup {
                    core: thread_core,
                    state: state2.clone(),
                    generation: generation_for_thread,
                    terminal: SessionState::Failed,
                };
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        warn!("session runtime build failed: {e}");
                        return;
                    }
                };
                let result = rt.block_on(run_session(
                    config,
                    cleanup.core.clone(),
                    state2,
                    params2,
                    stop_rx,
                    input_rx,
                    watchdog2,
                    watchdog_paused2,
                    cursor2,
                    cleanup.generation.value(),
                ));
                match result {
                    Ok(()) => cleanup.terminal = SessionState::Stopped,
                    Err(e) => warn!("session ended with error: {e:#}"),
                }
            }) {
            Ok(thread) => thread,
            Err(error) => {
                warn!(?error, "failed to spawn ct-session thread");
                core.close();
                if let Err(error) = core.release_session_lease(generation) {
                    warn!(?error, "failed to release rejected session generation");
                }
                set_state(&state, SessionState::Failed);
                return Ok(Self {
                    state,
                    params,
                    stop_tx,
                    input_tx,
                    watchdog,
                    watchdog_paused,
                    cursor,
                    thread: None,
                });
            }
        };

        Ok(Self {
            state,
            params,
            stop_tx,
            input_tx,
            watchdog,
            watchdog_paused,
            cursor,
            thread: Some(thread),
        })
    }

    fn rejected() -> Self {
        let state = Arc::new(AtomicU8::new(SessionState::Failed as u8));
        let (stop_tx, _) = mpsc::channel::<()>(1);
        let (input_tx, _) = mpsc::channel::<(u8, Vec<u8>)>(512);
        Self {
            state,
            params: Arc::new(std::sync::Mutex::new(None)),
            stop_tx,
            input_tx,
            watchdog: Arc::new(WatchdogStatus::default()),
            watchdog_paused: Arc::new(AtomicBool::new(false)),
            cursor: Arc::new(CursorShared::default()),
            thread: None,
        }
    }

    /// Handle for input threads; packets are dropped until the host's
    /// input channels open (`Streaming` state).
    pub fn input_sender(&self) -> InputSender {
        InputSender {
            tx: self.input_tx.clone(),
        }
    }

    /// M4 stall-watchdog readback (indicator + terminal reconnect flag).
    pub fn watchdog(&self) -> &Arc<WatchdogStatus> {
        &self.watchdog
    }

    /// M4 cursor readback (host visibility + last position) — see
    /// `CursorShared` for the atomic-store rationale.
    pub fn cursor(&self) -> &Arc<CursorShared> {
        &self.cursor
    }

    /// Hold/resume watchdog escalation around minimized/hidden phases
    /// (a hidden window legitimately stops mattering; it must not
    /// escalate). Idempotent; picked up on the next session tick.
    pub fn set_watchdog_paused(&self, paused: bool) {
        self.watchdog_paused.store(paused, Ordering::Release);
    }

    pub fn state(&self) -> SessionState {
        match self.state.load(Ordering::Acquire) {
            0 => SessionState::Connecting,
            1 => SessionState::PeerConnected,
            2 => SessionState::Streaming,
            3 => SessionState::Failed,
            _ => SessionState::Stopped,
        }
    }

    /// `ConnectionComplete` parameters once available.
    pub fn stream_params(&self) -> Option<StreamParams> {
        self.params
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Request shutdown and join the session thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.stop_tx.try_send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Internals ─────────────────────────────────────────────────────────────

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;

/// Peer/channel callback → session loop control events.
///
/// Video FEC payloads deliberately bypass this bounded queue and feed
/// `RxCore` directly. A single encoded frame spans many DataChannel messages;
/// routing those bursts through `try_send` could silently drop shards when the
/// 256-entry control queue filled, producing reference corruption and stalls.
enum LocalEvent {
    PeerState(RTCPeerConnectionState),
    LocalCandidate(RTCIceCandidateInit),
    /// A host-created input channel opened (id = `TransportChannelId`).
    InputOpen(u8, Arc<RTCDataChannel>),
}

/// DataChannel label → transport channel id for client → host input
/// (host creates the channels; `InboundPacket::encode` picks the id).
fn input_channel_id(label: &str) -> Option<u8> {
    use common::api_bindings::TransportChannelId as T;
    Some(match label {
        "mouse_reliable" => T::MOUSE_RELIABLE,
        "mouse_absolute" => T::MOUSE_ABSOLUTE,
        "mouse_relative" => T::MOUSE_RELATIVE,
        "keyboard" => T::KEYBOARD,
        "touch" => T::TOUCH,
        "controllers" => T::CONTROLLERS,
        _ => return None,
    })
}

/// Cloneable handle for UI/input threads: encodes an [`InboundPacket`]
/// and queues it for the session loop to send on the matching channel.
#[derive(Clone)]
pub struct InputSender {
    tx: mpsc::Sender<(u8, Vec<u8>)>,
}

impl InputSender {
    /// Returns `false` when the packet cannot be encoded or the session
    /// queue is full/gone (drop is the right behavior for input).
    pub fn send(&self, pkt: &InboundPacket) -> bool {
        let Some((ch, bytes)) = pkt.encode() else {
            return false;
        };
        self.tx.try_send((ch.0, bytes)).is_ok()
    }
}

/// Progress states only upgrade (Connecting → PeerConnected → Streaming):
/// `ConnectionComplete` can arrive on the WS *before* the peer-connected
/// callback fires (observed live 2026-07-14 — 130 ms inversion), and the
/// late PeerConnected must not demote Streaming. Terminal states
/// (Failed/Stopped) always win.
fn set_state(state: &AtomicU8, s: SessionState) {
    let new = s as u8;
    if matches!(s, SessionState::Failed | SessionState::Stopped) {
        state.store(new, Ordering::Release);
        return;
    }
    // Upgrade-only among progress states (0..=2).
    let _ = state.fetch_update(Ordering::Release, Ordering::Acquire, |cur| {
        if cur < new && cur <= SessionState::Streaming as u8 {
            Some(new)
        } else {
            None
        }
    });
}
struct SessionCleanup {
    core: Arc<RxCore>,
    state: Arc<AtomicU8>,
    generation: SessionGeneration,
    terminal: SessionState,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        // Wake all native waiters before exposing the terminal state.
        self.core.close();
        if let Err(error) = self.core.release_session_lease(self.generation) {
            warn!(?error, "failed to release session generation");
        }
        set_state(&self.state, self.terminal);
    }
}

fn queue_local_event(tx: &mpsc::Sender<LocalEvent>, overflowed: &AtomicBool, event: LocalEvent) {
    match tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            overflowed.store(true, Ordering::Release);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}
// ── Control-send retry ladder (G003) ──────────────────────────────────────
//
// A failed ACK/NeedsIdr/Subscribe DataChannel send must never silently drop
// the pending control (the existing re-latch semantics already keep it
// pending) and must never tear the session down on the first failure: the
// signaling WS path outlives a dead media path, so recovery intent is
// mirrored there immediately, then the DataChannel is retried on a bounded
// backoff before finally escalating. Pure/time-driven so it is unit
// testable without any networking.

/// Backoff ladder for a failed control-channel send, milliseconds since the
/// most recent failure.
const CONTROL_BACKOFF_MS: [u64; 3] = [100, 250, 500];

/// Action the session loop must perform for the current tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlRetryAction {
    /// Attempt the pending DataChannel control send again now.
    RetryDataChannel,
    /// Backoff ladder exhausted: request an ICE restart over signaling,
    /// then make one more DataChannel attempt.
    RestartIce,
    /// The post-ICE-restart attempt also failed: end the session.
    Fail,
}

/// Pure retry state machine for one control-send failure episode. `poll`
/// fires at most one action per call (mirrors [`crate::watchdog::StallWatchdog`]'s one-rung-
/// per-tick discipline): steps 0..3 are the 100/250/500 ms backoff
/// retries, step 3 escalates to `RestartIce`, step 4 is the one DataChannel
/// retry made after the ICE restart, and anything beyond that is terminal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ControlRetry {
    /// `Some(t)` = an episode is open; `t` is when the most recent failure
    /// (initial or retry) was recorded.
    failed_at_ms: Option<u64>,
    step: u8,
}

impl ControlRetry {
    /// Record a failed control send at `now_ms`. Returns `true` the first
    /// time this fires for a fresh episode — the caller mirrors
    /// `RequestIdr` over signaling exactly once on that edge.
    fn on_failure(&mut self, now_ms: u64) -> bool {
        let fresh = self.failed_at_ms.is_none();
        self.failed_at_ms = Some(now_ms);
        fresh
    }

    /// A control send just succeeded: close the episode and reset the
    /// ladder for the next one.
    fn on_success(&mut self) {
        *self = Self::default();
    }

    /// Whether a failure episode is currently open (sends are gated by
    /// [`ControlRetry::poll`] rather than attempted freely).
    fn is_open(&self) -> bool {
        self.failed_at_ms.is_some()
    }

    /// Poll once per tick. Returns the due action, if any.
    fn poll(&mut self, now_ms: u64) -> Option<ControlRetryAction> {
        let since = self.failed_at_ms?;
        match self.step {
            0..=2 => {
                let idx = self.step as usize;
                if now_ms.saturating_sub(since) >= CONTROL_BACKOFF_MS[idx] {
                    self.step += 1;
                    Some(ControlRetryAction::RetryDataChannel)
                } else {
                    None
                }
            }
            3 => {
                self.step += 1;
                Some(ControlRetryAction::RestartIce)
            }
            4 => {
                self.step += 1;
                Some(ControlRetryAction::RetryDataChannel)
            }
            _ => Some(ControlRetryAction::Fail),
        }
    }
}

/// Newest-wins outbound cumulative-ACK slot, keyed by FEC epoch: ACKs are
/// `highest_fully_decoded` (fec-framing.md §2), so a newer un-sent value
/// always supersedes an older one, and a value from a superseded epoch must
/// never be sent once renegotiation selects a new one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PendingAck {
    slot: Option<(Option<u32>, u32)>,
}

impl PendingAck {
    /// Latch a freshly polled ACK `value` for `epoch`. A different epoch
    /// always replaces the slot outright; the same epoch keeps the higher
    /// cumulative value.
    fn latch(&mut self, epoch: Option<u32>, value: u32) {
        self.slot = Some(match self.slot {
            Some((e, v)) if e == epoch => (epoch, v.max(value)),
            _ => (epoch, value),
        });
    }

    fn value(&self) -> Option<u32> {
        self.slot.map(|(_, v)| v)
    }

    fn clear(&mut self) {
        self.slot = None;
    }
}

/// Cadence for re-sending NeedsIdr while queue-owned decoder-recovery
/// ([`RxCore::recovery`]) stays open: fires immediately on open, then backs
/// off through the same 100/250/500 ms ladder and holds at the 500 ms cap
/// until [`RxCore::acknowledge_decoded_key`] closes recovery. Distinct from
/// [`ControlRetry`]: this cadence governs *when a resend is due*, not what
/// happens when a send attempt fails (that is still [`ControlRetry`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct NeedsIdrLatch {
    open: bool,
    last_sent_ms: Option<u64>,
    resends: usize,
}

impl NeedsIdrLatch {
    /// Recovery is open; a fresh open starts the cadence. Idempotent while
    /// already open (a same-episode re-signal must not reset the clock).
    fn open(&mut self) {
        if !self.open {
            self.open = true;
            self.last_sent_ms = None;
            self.resends = 0;
        }
    }

    /// Recovery closed (acknowledged): clear the latch — but never before a
    /// bare (no-open-recovery) signal has actually been sent once. Without
    /// that guard, a decode-flag NeedsIdr drained while sends are gated (open
    /// retry episode / broken channel) would be silently dropped when the
    /// next tick observes `recovery() == None` and force-closes the latch.
    fn close(&mut self) {
        if !self.open || self.last_sent_ms.is_some() {
            *self = Self::default();
        }
    }

    /// Whether a NeedsIdr control is due to (re)send at `now_ms`.
    fn due(&self, now_ms: u64) -> bool {
        if !self.open {
            return false;
        }
        match self.last_sent_ms {
            None => true,
            Some(last) => {
                let idx = self.resends.min(CONTROL_BACKOFF_MS.len() - 1);
                now_ms.saturating_sub(last) >= CONTROL_BACKOFF_MS[idx]
            }
        }
    }

    /// Record a successful send at `now_ms`.
    fn sent(&mut self, now_ms: u64) {
        if self.last_sent_ms.is_some() && self.resends < CONTROL_BACKOFF_MS.len() - 1 {
            self.resends += 1;
        }
        self.last_sent_ms = Some(now_ms);
    }
}

/// A control (ACK/NeedsIdr/Subscribe) DataChannel send just failed. Mirrors
/// recovery intent over signaling immediately on the first failure of a
/// fresh episode (the WS path stays alive even when the media path is
/// dead) — see [`ControlRetry`] for the backoff/escalation ladder that
/// follows.
async fn control_send_failed(
    ws_tx: &Arc<Mutex<WsSink>>,
    retry: &mut ControlRetry,
    now_ms: u64,
) -> anyhow::Result<()> {
    if retry.on_failure(now_ms) {
        warn!("control channel send failed — mirroring RequestIdr over signaling");
        send_ws(ws_tx, &StreamClientMessage::RequestIdr).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    config: SessionConfig,
    core: Arc<RxCore>,
    state: Arc<AtomicU8>,
    params_out: Arc<std::sync::Mutex<Option<StreamParams>>>,
    mut stop_rx: mpsc::Receiver<()>,
    mut input_rx: mpsc::Receiver<(u8, Vec<u8>)>,
    watchdog_status: Arc<WatchdogStatus>,
    watchdog_paused: Arc<AtomicBool>,
    cursor_shared: Arc<CursorShared>,
    // The real `SessionGeneration` lease value acquired in
    // `Session::try_start` (G004) — stamped into `dog.start()` and into
    // `watchdog_status`'s generation so incident telemetry and the
    // decode/present stall latches carry the actual lease identity instead
    // of a decorative self-generated counter.
    session_generation: u64,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let now_ms = move || started.elapsed().as_millis().min(u64::MAX as u128) as u64;

    // 1. Login (cookie session).
    let insecure = matches!(config.trust, ServerTrust::InsecureAcceptAny);
    // NOTE: reqwest cannot take our custom pinned verifier; with a pin we
    // still run the login handshake in accept-invalid mode and enforce the
    // pin on the WS handshake below (same server certificate). W2: replace
    // the login path with a raw rustls HTTP client so the pin covers both.
    let http = reqwest::Client::builder()
        .use_rustls_tls()
        .danger_accept_invalid_certs(true)
        .cookie_store(true)
        .build()
        .context("build http client")?;
    if !insecure {
        info!("TLS pin active: enforced on the signaling WS handshake");
    }

    let login_url = format!("{}/api/login", config.base_url);
    let resp = http
        .post(&login_url)
        .json(&serde_json::json!({
            "name": config.username,
            "password": config.password,
        }))
        .send()
        .await
        .context("login request")?;
    if !resp.status().is_success() {
        bail!("login failed: HTTP {}", resp.status());
    }
    let cookie_header = resp
        .cookies()
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect::<Vec<_>>()
        .join("; ");
    if cookie_header.is_empty() {
        bail!("login succeeded but no session cookie was set");
    }
    info!("login ok");

    // 2. Signaling WS.
    let ws_url = format!(
        "{}/api/host/stream",
        config
            .base_url
            .replace("https://", "wss://")
            .replace("http://", "ws://")
    );
    let mut request = ws_url.clone().into_client_request().context("ws request")?;
    request.headers_mut().insert(
        "Cookie",
        cookie_header.parse().context("cookie header value")?,
    );
    let connector = tokio_tungstenite::Connector::Rustls(client_config(config.trust.clone()));
    let (ws, _) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .context("ws connect")?;
    info!("signaling ws connected: {ws_url}");
    let (ws_tx, mut ws_rx) = ws.split();
    let ws_tx = Arc::new(Mutex::new(ws_tx));

    // 3. Flow machine + peer scaffolding.
    let mut flow = SignalingFlow::new(config.flow.clone());
    let api = build_webrtc_api()?;
    let (ev_tx, mut ev_rx) = mpsc::channel::<LocalEvent>(256);
    let event_overflowed = Arc::new(AtomicBool::new(false));
    let ack_open = Arc::new(std::sync::Mutex::new(None));

    let mut peer: Option<Arc<RTCPeerConnection>> = None;
    // Remote candidates arriving before the remote description is applied.
    let mut pending_candidates: Vec<RtcIceCandidate> = Vec::new();
    let mut remote_description_set = false;
    let mut ack_channel: Option<Arc<RTCDataChannel>> = None;
    let mut subscribe_sent = false;
    let mut pending_ack = PendingAck::default();
    let mut needs_idr = NeedsIdrLatch::default();
    let mut control_retry = ControlRetry::default();
    // Host-created input channels by TransportChannelId (opened async).
    let mut input_channels: std::collections::HashMap<u8, Arc<RTCDataChannel>> =
        std::collections::HashMap::new();

    send_ws(&ws_tx, &flow.init_message()).await?;

    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // G004 three-stage watchdog supervisor (watchdog.rs): the receive
    // stage is the original M4 mirror of the web ladder, armed at
    // ConnectionComplete and fed from the delivered-frame counter on this
    // 50 ms tick. Decode/present are new downstream stages fed ONLY from
    // native heartbeats (`RxCore::decoded_output_count`/`presented_count`)
    // — never from receive-side symbol/frame arrival. RequestIdr/RestartIce
    // go over the signaling socket (alive even when the media path is
    // dead); any stage's Reconnect is terminal — it flags WatchdogStatus
    // and ends the session as Failed so the shell rebuilds.
    let mut dog = WatchdogSupervisor::new(WatchdogConfig::default());
    let mut dog_frames: u64 = 0;
    let mut dog_decoded: u64 = 0;
    let mut dog_presented: u64 = 0;
    let mut dog_audio: u64 = 0;
    let mut dog_audio_ms: u64 = 0;
    let mut dog_last_idr_attempt: u32 = 0;
    let mut dog_paused = false;
    let mut last_ws_ping_ms: u64 = now_ms();

    loop {
        tokio::select! {
            _ = stop_rx.recv() => {
                info!("stop requested");
                break;
            }
            _ = tick.tick() => {
                let now = now_ms();
                core.tick(now);
                if now.saturating_sub(last_ws_ping_ms) >= 30_000 {
                    last_ws_ping_ms = now;
                    ws_keepalive(&ws_tx).await;
                }
                if event_overflowed.swap(false, Ordering::AcqRel) {
                    bail!("session control event queue saturated");
                }
                if ack_channel.is_none() {
                    ack_channel = ack_open
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                }

                // G003 control-send retry ladder: a closed episode always
                // allows sending; an open one only allows it once the
                // ladder says a retry is due (see `ControlRetry`).
                let mut may_send_control = true;
                if control_retry.is_open() {
                    may_send_control = false;
                    match control_retry.poll(now) {
                        Some(ControlRetryAction::RetryDataChannel) => {
                            may_send_control = true;
                        }
                        Some(ControlRetryAction::RestartIce) => {
                            warn!(
                                "control channel retry ladder exhausted — requesting ICE restart"
                            );
                            send_ws(&ws_tx, &StreamClientMessage::RestartIce).await?;
                        }
                        Some(ControlRetryAction::Fail) => {
                            bail!(
                                "control channel send failed after full retry ladder \
                                 (100/250/500ms backoff + ICE restart)"
                            );
                        }
                        None => {}
                    }
                }

                // A granted retry rung that finds nothing pending means the
                // failed control has since been superseded/satisfied: the
                // episode is moot and must close instead of marching to
                // RestartIce/Fail with zero attempts (bare NeedsIdr case).
                let retry_granted = may_send_control && control_retry.is_open();
                let mut attempted = false;
                if let Some(ch) = &ack_channel {
                    let mut channel_broken = false;

                    if may_send_control
                        && !channel_broken
                        && !subscribe_sent
                        && let Some(bytes) = core.control_message(FecControl::Subscribe)
                    {
                        attempted = true;
                        match ch.send(&bytes::Bytes::from(bytes)).await {
                            Ok(_) => {
                                subscribe_sent = true;
                                control_retry.on_success();
                                info!("video_fec subscribed");
                            }
                            Err(error) => {
                                warn!(?error, "send subscribe failed");
                                control_send_failed(&ws_tx, &mut control_retry, now).await?;
                                channel_broken = true;
                            }
                        }
                    }

                    if let Some(a) = core.poll_ack() {
                        pending_ack.latch(core.selected_epoch(), a);
                    }
                    if may_send_control
                        && !channel_broken
                        && let Some(value) = pending_ack.value()
                        && let Some(bytes) = core.control_message(FecControl::Ack(value))
                    {
                        attempted = true;
                        match ch.send(&bytes::Bytes::from(bytes)).await {
                            Ok(_) => {
                                pending_ack.clear();
                                control_retry.on_success();
                            }
                            Err(error) => {
                                warn!(?error, "send FEC ack failed");
                                control_send_failed(&ws_tx, &mut control_retry, now).await?;
                                channel_broken = true;
                            }
                        }
                    }

                    // Latched NeedsIdr: re-armed while queue-owned recovery
                    // stays open, cleared only when a decoder ack closes it
                    // (`RxCore::recovery()` -> `None`); a bare decode-flag
                    // signal without an open recovery still gets a single
                    // immediate send via the same latch (`close()` refuses to
                    // clear an unsent latch, so a send gated by an open retry
                    // episode or broken channel survives to the next tick).
                    if core.recovery().is_some() {
                        needs_idr.open();
                    } else {
                        needs_idr.close();
                    }
                    if core.poll_needs_idr() {
                        needs_idr.open();
                    }
                    if may_send_control
                        && !channel_broken
                        && needs_idr.due(now)
                        && let Some(bytes) =
                            core.control_message(FecControl::NeedsIdr { reason: 1 })
                    {
                        attempted = true;
                        match ch.send(&bytes::Bytes::from(bytes)).await {
                            Ok(_) => {
                                needs_idr.sent(now);
                                control_retry.on_success();
                            }
                            Err(error) => {
                                warn!(?error, "send FEC needs-IDR failed");
                                control_send_failed(&ws_tx, &mut control_retry, now).await?;
                            }
                        }
                    }
                }
                if retry_granted && !attempted {
                    control_retry.on_success();
                }

                // Watchdog drive: pause edge → progress signals → one rung.
                let paused = watchdog_paused.load(Ordering::Acquire);
                if paused != dog_paused {
                    dog_paused = paused;
                    if paused {
                        dog.pause();
                    } else {
                        dog.resume(now_ms());
                    }
                }
                let frames = core.frames_delivered();
                if frames != dog_frames {
                    dog_frames = frames;
                    if let Some(WatchdogAction::Recovered { stalled_ms }) =
                        dog.frame_received(now_ms())
                    {
                        watchdog_status.stalled.store(false, Ordering::Release);
                        info!(stalled_ms, "stall watchdog: recovered");
                    }
                }
                // Decode/present progress: native heartbeats ONLY — never
                // derived from receive-side symbol/frame arrival (G004).
                let decoded = core.decoded_output_count();
                if decoded != dog_decoded {
                    dog_decoded = decoded;
                    dog.decode_progress(now_ms());
                }
                let presented = core.presented_count();
                if presented != dog_presented {
                    dog_presented = presented;
                    dog.present_progress(now_ms());
                }
                let audio_pushed = core.audio_pushed_count();
                if audio_pushed != dog_audio {
                    dog_audio = audio_pushed;
                    dog_audio_ms = now_ms();
                }
                if let Some((stage, action, action_generation)) = dog.tick(now_ms())
                    && dog.is_current(action_generation)
                {
                    match (stage, action) {
                        (WatchdogStage::Receive, WatchdogAction::Stall) => {
                            watchdog_status.stalled.store(true, Ordering::Release);
                            let snapshot = build_incident_snapshot(
                                &core,
                                &dog,
                                now_ms(),
                                dog_last_idr_attempt,
                                dog_audio_ms,
                            );
                            warn!(
                                ?snapshot,
                                "stall watchdog: receive stage stalled — indicator up"
                            );
                            watchdog_status.record_action(stage, action, snapshot);
                        }
                        (WatchdogStage::Receive, WatchdogAction::RequestIdr { attempt }) => {
                            dog_last_idr_attempt = attempt;
                            info!(attempt, "stall watchdog: requesting IDR via signaling");
                            send_ws(&ws_tx, &StreamClientMessage::RequestIdr).await?;
                            let snapshot = build_incident_snapshot(
                                &core,
                                &dog,
                                now_ms(),
                                dog_last_idr_attempt,
                                dog_audio_ms,
                            );
                            watchdog_status.record_action(stage, action, snapshot);
                        }
                        (WatchdogStage::Receive, WatchdogAction::RestartIce) => {
                            info!("stall watchdog: requesting ICE restart");
                            send_ws(&ws_tx, &StreamClientMessage::RestartIce).await?;
                            let snapshot = build_incident_snapshot(
                                &core,
                                &dog,
                                now_ms(),
                                dog_last_idr_attempt,
                                dog_audio_ms,
                            );
                            watchdog_status.record_action(stage, action, snapshot);
                        }
                        (_, WatchdogAction::DecodeStall { attempt }) => {
                            info!(
                                attempt,
                                "stall watchdog: decode stage stalled — decoder flush + IDR"
                            );
                            let snapshot = build_incident_snapshot(
                                &core,
                                &dog,
                                now_ms(),
                                dog_last_idr_attempt,
                                dog_audio_ms,
                            );
                            watchdog_status.record_action(stage, action, snapshot);
                        }
                        (_, WatchdogAction::PresentStall { attempt }) => {
                            info!(
                                attempt,
                                "stall watchdog: present stage stalled — device recreate/fallback"
                            );
                            let snapshot = build_incident_snapshot(
                                &core,
                                &dog,
                                now_ms(),
                                dog_last_idr_attempt,
                                dog_audio_ms,
                            );
                            watchdog_status.record_action(stage, action, snapshot);
                        }
                        (_, WatchdogAction::Reconnect) => {
                            watchdog_status.reconnect.store(true, Ordering::Release);
                            let snapshot = build_incident_snapshot(
                                &core,
                                &dog,
                                now_ms(),
                                dog_last_idr_attempt,
                                dog_audio_ms,
                            );
                            watchdog_status.record_action(stage, action, snapshot);
                            bail!("stall watchdog: {stage:?} ladder exhausted — reconnect");
                        }
                        (_, WatchdogAction::Recovered { .. } | WatchdogAction::Stall) => {
                            // Unreachable by construction: `dog.tick()` never
                            // emits `Recovered` (only `frame_received` does,
                            // handled separately above), and a non-`Receive`
                            // stage never emits `Stall` (`Receive`'s own
                            // `Stall` is matched earlier). A future
                            // watchdog.rs change that violates this pairing
                            // must be caught in debug builds, not silently
                            // dropped.
                            debug_assert!(
                                false,
                                "unreachable watchdog stage/action pairing: {stage:?}/{action:?}"
                            );
                            warn!(?stage, ?action, "watchdog: unexpected Recovered/Stall pairing from dog.tick()");
                        }
                        // Unreachable by construction: `WatchdogSupervisor::tick`
                        // only ever pairs `Decode`/`Present` with their own
                        // typed stall/`Reconnect` actions (see watchdog.rs).
                        _ => {
                            debug_assert!(
                                false,
                                "unreachable watchdog stage/action pairing: {stage:?}/{action:?}"
                            );
                            warn!(?stage, ?action, "watchdog: unexpected stage/action pairing from dog.tick()");
                        }
                    }
                }
            }
            pkt = input_rx.recv() => {
                // Session owns the matching sender, so recv never yields
                // None while the loop runs; drop packets for channels
                // that have not opened yet.
                if let Some((ch, bytes)) = pkt
                    && let Some(dc) = input_channels.get(&ch)
                {
                    let _ = dc.send(&bytes::Bytes::from(bytes)).await;
                }
            }
            ev = ev_rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    LocalEvent::InputOpen(id, ch) => {
                        debug!("input channel {} open (id {id})", ch.label());
                        input_channels.insert(id, ch);
                    }
                    LocalEvent::LocalCandidate(init) => {
                        send_ws(&ws_tx, &StreamClientMessage::WebRtc(
                            StreamSignalingMessage::AddIceCandidate(RtcIceCandidate {
                                candidate: init.candidate,
                                sdp_mid: init.sdp_mid,
                                sdp_mline_index: init.sdp_mline_index,
                                username_fragment: init.username_fragment,
                            }),
                        )).await?;
                    }
                    LocalEvent::PeerState(s) => {
                        debug!("peer state: {s}");
                        match s {
                            RTCPeerConnectionState::Connected => {
                                set_state(&state, SessionState::PeerConnected);
                            }
                            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                                bail!("peer connection {s}");
                            }
                            _ => {}
                        }
                    }
                }
            }
            msg = ws_rx.next() => {
                let Some(msg) = msg else {
                    if flow.is_terminated() { break; }
                    bail!("signaling ws closed by server");
                };
                let msg = msg.context("ws receive")?;
                let text = match msg {
                    WsMessage::Text(t) => t,
                    WsMessage::Close(frame) => {
                        if flow.is_terminated() {
                            info!("ws closed after termination");
                            break;
                        }
                        bail!("ws closed: {frame:?}");
                    }
                    _ => continue,
                };
                let server_msg: StreamServerMessage =
                    serde_json::from_str(&text).context("parse server message")?;
                if let StreamServerMessage::DebugLog { message, ty } = &server_msg {
                    debug!(?ty, "server: {message}");
                }
                for action in flow.on_server_message(server_msg) {
                    match action {
                        FlowAction::Send(m) => send_ws(&ws_tx, &m).await?,
                        FlowAction::CreatePeer(ice_servers) => {
                            peer = Some(
                                create_peer(
                                    &api,
                                    ice_servers,
                                    ev_tx.clone(),
                                    event_overflowed.clone(),
                                    ack_open.clone(),
                                    core.clone(),
                                    cursor_shared.clone(),
                                    started,
                                )
                                .await?,
                            );
                        }
                        FlowAction::ApplyRemoteOffer(desc) => {
                            let p = peer.as_ref().context("offer before Setup")?;
                            let answer = apply_offer_make_answer(p, desc).await?;
                            remote_description_set = true;
                            for cand in pending_candidates.drain(..) {
                                add_remote_candidate(p, cand).await?;
                            }
                            send_ws(&ws_tx, &StreamClientMessage::WebRtc(
                                StreamSignalingMessage::Description(answer),
                            )).await?;
                        }
                        FlowAction::AddRemoteCandidate(cand) => {
                            match peer.as_ref() {
                                Some(p) if remote_description_set => {
                                    add_remote_candidate(p, cand).await?;
                                }
                                _ => pending_candidates.push(cand),
                            }
                        }
                        FlowAction::Complete(params) => {
                            info!(?params, "ConnectionComplete");
                            if !core.configure_fec(params.fec_protocol_version, params.fec_epoch) {
                                bail!(
                                    "invalid negotiated FEC parameters: version={}, epoch={:?}",
                                    params.fec_protocol_version,
                                    params.fec_epoch
                                );
                            }

                            let channels = negotiated_channels(params.audio_channel_count);
                            if !(1..=8).contains(&params.audio_channel_count) {
                                warn!(
                                    audio_channel_count = params.audio_channel_count,
                                    fallback_channels = channels,
                                    "invalid/zero SDP audio channel count — falling back to stereo"
                                );
                            }
                            core.set_audio_channels(channels);
                            *params_out
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(params);
                            set_state(&state, SessionState::Streaming);
                            // Arm the watchdog: media is expected from here
                            // on; a stream that never delivers escalates.
                            // Stamp the real session lease generation (G004)
                            // — start() unconditionally resets `paused` on
                            // all three stage clocks, so a pause request
                            // still active here (e.g. minimize-during-
                            // (re)connect) MUST be re-applied right after,
                            // or escalation silently resumes while hidden.
                            dog.start(now_ms(), session_generation);
                            watchdog_status.set_generation(session_generation);
                            if dog_paused {
                                dog.pause();
                            }
                            dog_frames = core.frames_delivered();
                            dog_decoded = core.decoded_output_count();
                            dog_presented = core.presented_count();
                            dog_audio = core.audio_pushed_count();
                            dog_audio_ms = now_ms();
                            dog_last_idr_attempt = 0;
                        }
                        FlowAction::ProtocolError(error) => {
                            bail!("signaling protocol error: {error:?}");
                        }
                        FlowAction::Terminated { error_code } => {
                            info!(error_code, "ConnectionTerminated");
                            if let Some(peer) = peer.take() {
                                let _ = peer.close().await;
                            }
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    if let Some(peer) = peer {
        let _ = peer.close().await;
    }
    Ok(())
}
/// Build one G004 incident snapshot from current session/watchdog state —
/// called whenever any watchdog stage takes action (see the `dog.tick()`
/// handling in `run_session`). `generation` reads `dog.generation()`,
/// which is the real `SessionGeneration` lease value stamped into `dog` at
/// `dog.start()` (see `FlowAction::Complete` handling) — stable for the
/// life of this `run_session`, distinct across reconnects since each
/// `Session::try_start` acquires a fresh lease.
fn build_incident_snapshot(
    core: &RxCore,
    dog: &WatchdogSupervisor,
    now: u64,
    last_idr_attempt: u32,
    audio_last_ms: u64,
) -> IncidentSnapshot {
    let fec = core.video_stats();
    let (recovery_open, recovery_generation, recovery_epoch) = match core.recovery() {
        Some((generation, epoch)) => (true, generation, epoch),
        None => (false, 0, 0),
    };
    let (active_epoch, active_epoch_present) = match core.selected_epoch() {
        Some(epoch) => (epoch, true),
        None => (0, false),
    };
    let (last_ack, last_ack_present) = match core.last_ack_seen() {
        Some(value) => (value, true),
        None => (0, false),
    };
    let (last_frame_id, last_frame_id_present) = match core.last_frame_id() {
        Some(value) => (value, true),
        None => (0, false),
    };
    IncidentSnapshot {
        generation: dog.generation(),
        last_frame_id,
        last_frame_id_present,
        active_epoch,
        active_epoch_present,
        source_symbols_received: fec.source_symbols_received,
        repair_symbols_received: fec.repair_symbols_received,
        symbols_recovered: fec.symbols_recovered,
        frames_recovered: fec.frames_recovered,
        frames_dropped_awaiting_idr: fec.frames_dropped_awaiting_idr,
        loss_spans: fec.loss_spans,
        loss_spans_recovered: fec.loss_spans_recovered,
        queue_len: core.queue_len() as u32,
        last_ack,
        last_ack_present,
        last_idr_attempt,
        receive_age_ms: dog.receive_age_ms(now),
        decode_age_ms: dog.decode_age_ms(now),
        present_age_ms: dog.present_age_ms(now),
        audio_age_ms: now.saturating_sub(audio_last_ms),
        recovery_open,
        recovery_generation,
        recovery_epoch,
    }
}

async fn send_ws(ws_tx: &Arc<Mutex<WsSink>>, msg: &StreamClientMessage) -> anyhow::Result<()> {
    let text = serde_json::to_string(msg).context("serialise client message")?;
    ws_tx
        .lock()
        .await
        .send(WsMessage::Text(text))
        .await
        .context("ws send")
}

/// NAT keepalive for the signaling socket: during a healthy WebRTC
/// session this TCP connection is near-silent (media rides separate UDP
/// flows), and consumer routers expire idle TCP mappings after ~5 min —
/// two 2026-07-17 live sessions both lost the socket ~275 s in ("peer
/// closed connection without close_notify"), which also killed the
/// stall-watchdog's IDR/RestartIce path. A ws Ping every 30 s keeps the
/// mapping warm; failures are logged, not fatal (the receive side
/// surfaces the real error).
async fn ws_keepalive(ws_tx: &Arc<Mutex<WsSink>>) {
    if let Err(e) = ws_tx.lock().await.send(WsMessage::Ping(Vec::new())).await {
        warn!(err = %e, "signaling ws keepalive ping failed");
    }
}

fn build_webrtc_api() -> anyhow::Result<API> {
    let mut media = MediaEngine::default();
    media
        .register_default_codecs()
        .map_err(|e| anyhow::anyhow!("register codecs: {e}"))?;
    let registry = register_default_interceptors(Registry::new(), &mut media)
        .map_err(|e| anyhow::anyhow!("register interceptors: {e}"))?;
    Ok(APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build())
}

type EventFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

fn done() -> EventFuture {
    Box::pin(ready(()))
}

async fn create_peer(
    api: &API,
    ice_servers: Vec<RtcIceServer>,
    ev_tx: mpsc::Sender<LocalEvent>,
    event_overflowed: Arc<AtomicBool>,
    ack_open: Arc<std::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
    core: Arc<RxCore>,
    cursor_shared: Arc<CursorShared>,
    started: Instant,
) -> anyhow::Result<Arc<RTCPeerConnection>> {
    let rtc_config = RTCConfiguration {
        ice_servers: ice_servers
            .into_iter()
            .map(|s| RTCIceServer {
                urls: s.urls,
                username: s.username,
                credential: s.credential,
            })
            .collect(),
        ..Default::default()
    };
    let peer = Arc::new(
        api.new_peer_connection(rtc_config)
            .await
            .map_err(|e| anyhow::anyhow!("new peer connection: {e}"))?,
    );

    let tx = ev_tx.clone();
    let overflowed = event_overflowed.clone();
    peer.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        queue_local_event(&tx, &overflowed, LocalEvent::PeerState(s));
        done()
    }));

    let tx = ev_tx.clone();
    let overflowed = event_overflowed.clone();
    peer.on_ice_candidate(Box::new(move |cand: Option<RTCIceCandidate>| {
        if let Some(cand) = cand {
            match cand.to_json() {
                Ok(init) => queue_local_event(
                    &tx,
                    &overflowed,
                    LocalEvent::LocalCandidate(RTCIceCandidateInit {
                        candidate: init.candidate,
                        sdp_mid: init.sdp_mid,
                        sdp_mline_index: init.sdp_mline_index,
                        username_fragment: init.username_fragment,
                    }),
                ),
                Err(e) => warn!("ice candidate to_json failed: {e}"),
            }
        }
        done()
    }));

    let tx = ev_tx.clone();
    let video_core = core.clone();
    peer.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let label = dc.label().to_string();
        debug!("data channel from host: \"{label}\"");
        match label.as_str() {
            "video_fec" => {
                let core = video_core.clone();
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    core.on_message(
                        &msg.data,
                        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    );
                    done()
                }));
            }
            "video_fec_ack" => {
                let ack_open = ack_open.clone();
                let dc2 = dc.clone();
                let publish_open = move || {
                    *ack_open
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dc2.clone());
                };
                if dc.ready_state() == RTCDataChannelState::Open {
                    publish_open();
                } else {
                    dc.on_open(Box::new(move || {
                        publish_open();
                        done()
                    }));
                }
            }
            "cursor" => {
                // Direct store into `CursorShared`, not routed through
                // `LocalEvent`: see the struct's doc comment for why
                // (POS = pure atomics on the mouse-move hot path; SHAPE =
                // low-rate Mutex slot, sent only on shape change).
                let cursor_shared = cursor_shared.clone();
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    if let Some(pos) = decode_pos(&msg.data) {
                        cursor_shared.store(pos);
                    } else if let Some(shape) = decode_shape(&msg.data) {
                        cursor_shared.store_shape(shape);
                    }
                    done()
                }));
            }
            _ => {
                if let Some(id) = input_channel_id(&label) {
                    let tx = tx.clone();
                    let overflowed = event_overflowed.clone();
                    let dc2 = dc.clone();
                    if dc.ready_state() == RTCDataChannelState::Open {
                        queue_local_event(&tx, &overflowed, LocalEvent::InputOpen(id, dc2));
                    } else {
                        dc.on_open(Box::new(move || {
                            queue_local_event(
                                &tx,
                                &overflowed,
                                LocalEvent::InputOpen(id, dc2.clone()),
                            );
                            done()
                        }));
                    }
                }
                // Remaining control channels (general/stats/…) — unused.
            }
        }
        done()
    }));

    // Incoming RTP tracks: opus audio feeds the receive core's sample
    // queue (RFC 7587 — one opus packet per RTP payload, so no
    // depacketizer stage). The video track is discard-read: the default
    // video path stays on the FEC DataChannel duplicate, but not reading
    // would stall the interceptor pipeline.
    peer.on_track(Box::new(move |track, _receiver, _transceiver| {
        let core = core.clone();
        Box::pin(async move {
            let mime = track.codec().capability.mime_type.to_ascii_lowercase();
            if mime == "audio/opus" {
                debug!("track from host: {mime} — feeding audio queue");
                tokio::spawn(async move {
                    while let Ok((pkt, _)) = track.read_rtp().await {
                        if !pkt.payload.is_empty() {
                            core.push_audio(pkt.payload.to_vec());
                        }
                    }
                });
            } else {
                debug!("track from host: {mime} — discard-reading");
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1500];
                    while track.read(&mut buf).await.is_ok() {}
                });
            }
        })
    }));

    Ok(peer)
}

async fn apply_offer_make_answer(
    peer: &Arc<RTCPeerConnection>,
    desc: RtcSessionDescription,
) -> anyhow::Result<RtcSessionDescription> {
    let remote = match desc.ty {
        RtcSdpType::Offer => RTCSessionDescription::offer(desc.sdp),
        RtcSdpType::Answer => RTCSessionDescription::answer(desc.sdp),
        RtcSdpType::Pranswer => RTCSessionDescription::pranswer(desc.sdp),
        other => bail!("unexpected remote description type: {other:?}"),
    }
    .map_err(|e| anyhow::anyhow!("build remote description: {e}"))?;

    peer.set_remote_description(remote)
        .await
        .map_err(|e| anyhow::anyhow!("set remote description: {e}"))?;

    let answer = peer
        .create_answer(None)
        .await
        .map_err(|e| anyhow::anyhow!("create answer: {e}"))?;
    peer.set_local_description(answer)
        .await
        .map_err(|e| anyhow::anyhow!("set local description: {e}"))?;
    let local = peer
        .local_description()
        .await
        .context("local description missing after set")?;

    Ok(RtcSessionDescription {
        ty: from_webrtc_sdp(local.sdp_type),
        sdp: local.sdp,
    })
}

async fn add_remote_candidate(
    peer: &Arc<RTCPeerConnection>,
    cand: RtcIceCandidate,
) -> anyhow::Result<()> {
    peer.add_ice_candidate(RTCIceCandidateInit {
        candidate: cand.candidate,
        sdp_mid: cand.sdp_mid,
        sdp_mline_index: cand.sdp_mline_index,
        username_fragment: cand.username_fragment,
    })
    .await
    .map_err(|e| anyhow::anyhow!("add ice candidate: {e}"))
}

fn from_webrtc_sdp(value: RTCSdpType) -> RtcSdpType {
    match value {
        RTCSdpType::Offer => RtcSdpType::Offer,
        RTCSdpType::Answer => RtcSdpType::Answer,
        RTCSdpType::Pranswer => RtcSdpType::Pranswer,
        RTCSdpType::Rollback => RtcSdpType::Rollback,
        RTCSdpType::Unspecified => RtcSdpType::Unspecified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::api_bindings::TransportChannelId as T;
    // ── WatchdogStatus latch seam (G004) ────────────────────────────────

    #[test]
    fn watchdog_status_poll_decode_and_present_stall_are_poll_and_clear() {
        let status = WatchdogStatus::default();
        status.set_generation(1);
        status.record_action(
            WatchdogStage::Decode,
            WatchdogAction::DecodeStall { attempt: 2 },
            IncidentSnapshot {
                generation: 1,
                ..Default::default()
            },
        );
        assert_eq!(status.poll_decode_stall(), Some(2));
        assert_eq!(
            status.poll_decode_stall(),
            None,
            "poll-and-clear: a second poll must observe nothing"
        );
        assert_eq!(
            status.poll_present_stall(),
            None,
            "unrelated stage untouched"
        );
    }

    #[test]
    fn watchdog_status_decode_and_present_latches_are_independent_in_poll_order() {
        // Mirrors `ct_session_poll_watchdog_action`'s check order
        // (decode before present, capi.rs): both latches can be pending at
        // once and each poll-and-clears only its own stage.
        let status = WatchdogStatus::default();
        status.set_generation(1);
        status.record_action(
            WatchdogStage::Decode,
            WatchdogAction::DecodeStall { attempt: 1 },
            IncidentSnapshot {
                generation: 1,
                ..Default::default()
            },
        );
        status.record_action(
            WatchdogStage::Present,
            WatchdogAction::PresentStall { attempt: 4 },
            IncidentSnapshot {
                generation: 1,
                ..Default::default()
            },
        );
        assert_eq!(status.poll_decode_stall(), Some(1), "decode observed first");
        assert_eq!(
            status.poll_present_stall(),
            Some(4),
            "present latch survives an unrelated decode poll"
        );
    }

    #[test]
    fn watchdog_status_drops_a_stall_latch_recorded_under_a_stale_generation() {
        // The real, reachable async-staleness path (see watchdog.rs's
        // `is_current_detects_a_generation_change_between_two_start_calls`
        // for the primitive-level contract this repurposes): a
        // `DecodeStall` latched under generation 1 must be dropped, not
        // delivered, once `dog.start()` has moved the session on to
        // generation 2 — the genuinely asynchronous cross-thread consumer
        // (native pump polling via `ct_session_poll_watchdog_action`) must
        // never observe a stale action.
        let status = WatchdogStatus::default();
        status.set_generation(1);
        status.record_action(
            WatchdogStage::Decode,
            WatchdogAction::DecodeStall { attempt: 3 },
            IncidentSnapshot {
                generation: 1,
                ..Default::default()
            },
        );
        // A later dog.start() (e.g. a reconnect episode) bumps the live
        // generation before the native pump gets around to polling.
        status.set_generation(2);
        assert_eq!(
            status.poll_decode_stall(),
            None,
            "a latch recorded under a superseded generation must be dropped"
        );
        assert_eq!(
            status.poll_decode_stall(),
            None,
            "still gone on a second poll"
        );
    }

    #[test]
    fn watchdog_status_note_counter_increments_drive_supervisor_progress() {
        // note_decoded_output/note_presented (capi.rs) feed
        // decoded_output_count/presented_count, which `run_session` diffs
        // to drive `dog.decode_progress`/`dog.present_progress` — pin the
        // counter-delta contract those call sites depend on directly here.
        let core = crate::capi::RxCore::new(0);
        assert_eq!(core.decoded_output_count(), 0);
        assert_eq!(core.presented_count(), 0);
        core.note_decoded_output();
        core.note_decoded_output();
        core.note_presented();
        assert_eq!(core.decoded_output_count(), 2);
        assert_eq!(core.presented_count(), 1);
    }

    #[test]
    fn input_labels_map_to_wire_channel_ids() {
        // The host's INPUT_CHANNELS labels (streamer webrtc/mod.rs) must
        // land on the ids InboundPacket::encode targets.
        assert_eq!(input_channel_id("mouse_reliable"), Some(T::MOUSE_RELIABLE));
        assert_eq!(input_channel_id("mouse_absolute"), Some(T::MOUSE_ABSOLUTE));
        assert_eq!(input_channel_id("mouse_relative"), Some(T::MOUSE_RELATIVE));
        assert_eq!(input_channel_id("keyboard"), Some(T::KEYBOARD));
        assert_eq!(input_channel_id("touch"), Some(T::TOUCH));
        assert_eq!(input_channel_id("controllers"), Some(T::CONTROLLERS));
        assert_eq!(input_channel_id("general"), None);
        assert_eq!(input_channel_id("video_fec"), None);
        assert_eq!(input_channel_id("controller0"), None); // not in A2 scope
    }

    #[test]
    fn input_sender_encodes_and_drops_on_overflow() {
        let (tx, mut rx) = mpsc::channel::<(u8, Vec<u8>)>(2);
        let s = InputSender { tx };
        let pkt = InboundPacket::MouseMove {
            delta_x: 3,
            delta_y: -4,
        };
        assert!(s.send(&pkt));
        let (ch, bytes) = rx.try_recv().expect("queued");
        assert_eq!(ch, T::MOUSE_RELATIVE);
        // kind=0, i16 BE deltas — byte-exact wire (input_wire encode).
        assert_eq!(bytes, vec![0, 0, 3, 0xFF, 0xFC]);

        // Overflow: capacity 2 → the third unread send reports a drop.
        assert!(s.send(&pkt));
        assert!(s.send(&pkt));
        assert!(!s.send(&pkt), "full queue drops instead of blocking");
    }
    #[test]
    fn saturated_control_event_queue_latches_terminal_failure() {
        let (tx, _rx) = mpsc::channel(1);
        let overflowed = AtomicBool::new(false);
        queue_local_event(
            &tx,
            &overflowed,
            LocalEvent::PeerState(RTCPeerConnectionState::Connected),
        );
        queue_local_event(
            &tx,
            &overflowed,
            LocalEvent::PeerState(RTCPeerConnectionState::Failed),
        );
        assert!(
            overflowed.swap(false, Ordering::AcqRel),
            "a dropped control event must make the session fail rather than continue"
        );
    }
    #[test]
    fn cleanup_closes_queues_before_publishing_failure() {
        let core = Arc::new(RxCore::new(0));
        let generation = core.acquire_session_lease().expect("lease");
        let state = Arc::new(AtomicU8::new(SessionState::Connecting as u8));

        drop(SessionCleanup {
            core: core.clone(),
            state: state.clone(),
            generation,
            terminal: SessionState::Failed,
        });

        assert_eq!(state.load(Ordering::Acquire), SessionState::Failed as u8);
        assert!(
            core.wait_frame(Duration::ZERO).is_none(),
            "closed queue wakes frame consumers before Failed is observable"
        );
        assert!(
            core.wait_audio(Duration::ZERO).is_none(),
            "closed queue wakes audio consumers before Failed is observable"
        );
    }
    #[test]
    fn concurrent_session_start_is_rejected_without_touching_owner() {
        let core = Arc::new(RxCore::new(0));
        let generation = core.acquire_session_lease().expect("first lease");
        let session = Session::start(
            SessionConfig {
                base_url: "http://unused".into(),
                username: "unused".into(),
                password: "unused".into(),
                trust: ServerTrust::InsecureAcceptAny,
                flow: FlowConfig {
                    host_id: 0,
                    app_id: 0,
                    video_frame_queue_size: 1,
                    audio_sample_queue_size: 1,
                    bitrate_kbps: 1,
                    width: 1,
                    height: 1,
                    fps: 1,
                    supported_codecs: H264_BIT,
                },
            },
            core.clone(),
        );

        assert_eq!(session.state(), SessionState::Failed);
        core.release_session_lease(generation)
            .expect("rejected start must not release the owner lease");
    }
    #[test]
    fn control_retry_ladder_progression_and_success_reset() {
        let mut r = ControlRetry::default();
        assert!(!r.is_open());
        assert_eq!(r.poll(0), None, "no episode open yet");

        assert!(r.on_failure(0), "first failure is fresh");
        assert!(
            !r.on_failure(0),
            "second failure of the same episode is not fresh"
        );
        assert!(r.is_open());

        // Not due before 100ms.
        assert_eq!(r.poll(99), None);
        assert_eq!(r.poll(100), Some(ControlRetryAction::RetryDataChannel));

        // That retry itself fails -> clock resets, next due at +250ms.
        r.on_failure(100);
        assert_eq!(r.poll(349), None);
        assert_eq!(r.poll(350), Some(ControlRetryAction::RetryDataChannel));

        // Second retry fails -> next due at +500ms.
        r.on_failure(350);
        assert_eq!(r.poll(849), None);
        assert_eq!(r.poll(850), Some(ControlRetryAction::RetryDataChannel));

        // Third retry fails -> ladder exhausted: escalate to ICE restart
        // immediately (no further wait), then one more DataChannel attempt.
        r.on_failure(850);
        assert_eq!(r.poll(850), Some(ControlRetryAction::RestartIce));
        assert_eq!(r.poll(850), Some(ControlRetryAction::RetryDataChannel));

        // The post-ICE attempt fails too -> terminal, and stays terminal.
        r.on_failure(850);
        assert_eq!(r.poll(850), Some(ControlRetryAction::Fail));
        assert_eq!(r.poll(999_999), Some(ControlRetryAction::Fail));

        // Success at any point fully resets the ladder for the next episode.
        r.on_success();
        assert!(!r.is_open());
        assert_eq!(r.poll(1_000_000), None);
        assert!(r.on_failure(1_000_000), "fresh again after a reset");
    }

    #[test]
    fn control_retry_escalation_order() {
        let mut r = ControlRetry::default();
        let mut seq = Vec::new();
        let mut t = 0u64;
        r.on_failure(t);
        for _ in 0..6 {
            t += 1_000; // always past any backoff threshold
            if let Some(action) = r.poll(t) {
                seq.push(action);
                if action != ControlRetryAction::RestartIce {
                    r.on_failure(t);
                }
                if action == ControlRetryAction::Fail {
                    break;
                }
            }
        }
        assert_eq!(
            seq,
            vec![
                ControlRetryAction::RetryDataChannel,
                ControlRetryAction::RetryDataChannel,
                ControlRetryAction::RetryDataChannel,
                ControlRetryAction::RestartIce,
                ControlRetryAction::RetryDataChannel,
                ControlRetryAction::Fail,
            ]
        );
    }

    #[test]
    fn pending_ack_epoch_newest_wins() {
        let mut ack = PendingAck::default();
        assert_eq!(ack.value(), None);

        ack.latch(Some(5), 10);
        assert_eq!(ack.value(), Some(10));

        // Same epoch, a lower/out-of-order value must not clobber a higher
        // pending one downward (cumulative ACK — newest/highest wins).
        ack.latch(Some(5), 7);
        assert_eq!(ack.value(), Some(10));

        // Same epoch, newer higher value wins.
        ack.latch(Some(5), 20);
        assert_eq!(ack.value(), Some(20));

        // A different (renegotiated) epoch always replaces the stale slot,
        // even though its raw ack value is numerically lower — a stale
        // epoch's ACK must never be sent after renegotiation.
        ack.latch(Some(6), 1);
        assert_eq!(ack.value(), Some(1));

        ack.clear();
        assert_eq!(ack.value(), None);
    }

    #[test]
    fn needs_idr_latch_persists_while_recovery_open_then_stops_after_ack() {
        let mut latch = NeedsIdrLatch::default();
        assert!(!latch.due(0), "closed latch is never due");

        latch.open();
        assert!(latch.due(0), "fires immediately on open");
        latch.sent(0);
        assert!(!latch.due(50));
        assert!(latch.due(100), "first re-send due at +100ms");
        latch.sent(100);
        assert!(!latch.due(300));
        assert!(latch.due(350), "second re-send due at +250ms after that");
        latch.sent(350);
        assert!(!latch.due(700));
        assert!(latch.due(850), "capped at 500ms cadence thereafter");
        latch.sent(850);
        assert!(latch.due(1_350), "cadence holds at the 500ms cap");

        // acknowledge_decoded_key closing recovery clears the latch.
        latch.close();
        assert!(!latch.due(2_000));
    }

    #[test]
    fn needs_idr_latch_reopen_is_idempotent_mid_episode() {
        let mut latch = NeedsIdrLatch::default();
        latch.open();
        latch.sent(0);
        latch.open(); // still the same episode: must not reset the cadence
        assert!(!latch.due(50));
        assert!(latch.due(100));
    }

    #[test]
    fn needs_idr_latch_close_never_drops_an_unsent_bare_signal() {
        // A bare decode-flag NeedsIdr opens the latch without an open
        // recovery. If sends are gated that tick (retry episode / broken
        // channel), the next tick's `close()` (recovery() is None) must NOT
        // silently drop the still-unsent control.
        let mut latch = NeedsIdrLatch::default();
        latch.open();
        latch.close(); // unsent: refused
        assert!(latch.due(0), "unsent bare signal survives close()");

        latch.sent(0);
        latch.close(); // sent once: clears normally
        assert_eq!(latch, NeedsIdrLatch::default());
        assert!(!latch.due(1000));
    }
}
