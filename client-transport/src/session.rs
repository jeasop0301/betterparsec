//! Async client session: login → WS signaling → WebRTC peer (answerer) →
//! `video_fec` subscribe → [`RxCore`] feed (m6-native-spike.md §F).
//!
//! Threading: [`Session::start`] spawns a dedicated tokio runtime thread;
//! the caller (C ABI or ct-probe) pulls frames through the shared
//! [`RxCore`] and stops via [`Session::stop`].

use std::future::ready;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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

use crate::capi::RxCore;
use crate::cursor::{CursorShared, decode_pos, decode_shape};
use crate::flow::{FlowAction, FlowConfig, SignalingFlow, StreamParams, negotiated_channels};
use crate::tls::{ServerTrust, client_config};
use crate::watchdog::{StallWatchdog, WatchdogAction, WatchdogConfig};

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

/// M4 stall-watchdog readback shared with the shell: indicator state +
/// terminal reconnect request (the shell tears the session down and
/// rebuilds — mirrors the web wiring's reconnect rung).
#[derive(Debug, Default)]
pub struct WatchdogStatus {
    stalled: AtomicBool,
    reconnect: AtomicBool,
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
    pub fn start(config: SessionConfig, core: Arc<RxCore>) -> Self {
        let state = Arc::new(AtomicU8::new(SessionState::Connecting as u8));
        let params = Arc::new(std::sync::Mutex::new(None));
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);
        // Input backlog ~= a few frames of events; overflow drops (stale
        // input is worse than lost input).
        let (input_tx, input_rx) = mpsc::channel::<(u8, Vec<u8>)>(512);
        let watchdog = Arc::new(WatchdogStatus::default());
        let watchdog_paused = Arc::new(AtomicBool::new(false));
        let cursor = Arc::new(CursorShared::default());

        let state2 = state.clone();
        let params2 = params.clone();
        let watchdog2 = watchdog.clone();
        let watchdog_paused2 = watchdog_paused.clone();
        let cursor2 = cursor.clone();
        let thread = std::thread::Builder::new()
            .name("ct-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        warn!("session runtime build failed: {e}");
                        state2.store(SessionState::Failed as u8, Ordering::Release);
                        return;
                    }
                };
                let result = rt.block_on(run_session(
                    config,
                    core,
                    state2.clone(),
                    params2,
                    stop_rx,
                    input_rx,
                    watchdog2,
                    watchdog_paused2,
                    cursor2,
                ));
                match result {
                    Ok(()) => state2.store(SessionState::Stopped as u8, Ordering::Release),
                    Err(e) => {
                        warn!("session ended with error: {e:#}");
                        state2.store(SessionState::Failed as u8, Ordering::Release);
                    }
                }
            })
            .expect("spawn ct-session thread");

        Self {
            state,
            params,
            stop_tx,
            input_tx,
            watchdog,
            watchdog_paused,
            cursor,
            thread: Some(thread),
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

/// Peer/channel callback → session loop events.
enum LocalEvent {
    PeerState(RTCPeerConnectionState),
    LocalCandidate(RTCIceCandidateInit),
    FecData(Vec<u8>),
    FecAckOpen(Arc<RTCDataChannel>),
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

    let mut peer: Option<Arc<RTCPeerConnection>> = None;
    // Remote candidates arriving before the remote description is applied.
    let mut pending_candidates: Vec<RtcIceCandidate> = Vec::new();
    let mut remote_description_set = false;
    let mut ack_channel: Option<Arc<RTCDataChannel>> = None;
    // Host-created input channels by TransportChannelId (opened async).
    let mut input_channels: std::collections::HashMap<u8, Arc<RTCDataChannel>> =
        std::collections::HashMap::new();

    send_ws(&ws_tx, &flow.init_message()).await?;

    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // M4 stall watchdog (Rust mirror of the web ladder, watchdog.rs):
    // armed at ConnectionComplete, fed from the delivered-frame counter on
    // this 50 ms tick. RequestIdr/RestartIce go over the signaling socket
    // (alive even when the media path is dead); Reconnect is terminal —
    // it flags WatchdogStatus and ends the session as Failed so the shell
    // rebuilds.
    let mut dog = StallWatchdog::new(WatchdogConfig::default());
    let mut dog_frames: u64 = 0;
    let mut dog_paused = false;

    loop {
        tokio::select! {
            _ = stop_rx.recv() => {
                info!("stop requested");
                break;
            }
            _ = tick.tick() => {
                core.tick(now_ms());
                if let Some(ch) = &ack_channel {
                    if let Some(a) = core.poll_ack() {
                        let _ = ch.send(&bytes::Bytes::copy_from_slice(&a.to_le_bytes())).await;
                    }
                    if core.poll_needs_idr() {
                        let _ = ch.send(&bytes::Bytes::from_static(&[0x00])).await;
                    }
                }

                // Watchdog drive: pause edge → frame signal → one rung.
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
                match dog.tick(now_ms()) {
                    Some(WatchdogAction::Stall) => {
                        watchdog_status.stalled.store(true, Ordering::Release);
                        warn!("stall watchdog: no frames delivered — indicator up");
                    }
                    Some(WatchdogAction::RequestIdr { attempt }) => {
                        info!(attempt, "stall watchdog: requesting IDR via signaling");
                        send_ws(&ws_tx, &StreamClientMessage::RequestIdr).await?;
                    }
                    Some(WatchdogAction::RestartIce) => {
                        info!("stall watchdog: requesting ICE restart");
                        send_ws(&ws_tx, &StreamClientMessage::RestartIce).await?;
                    }
                    Some(WatchdogAction::Reconnect) => {
                        watchdog_status.reconnect.store(true, Ordering::Release);
                        bail!("stall watchdog: ladder exhausted — reconnect");
                    }
                    Some(WatchdogAction::Recovered { .. }) | None => {}
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
                    LocalEvent::FecData(data) => {
                        core.on_message(&data, now_ms());
                    }
                    LocalEvent::FecAckOpen(ch) => {
                        // Subscribe: activates the host FEC sender.
                        ch.send(&bytes::Bytes::from_static(&[0x01]))
                            .await
                            .context("send subscribe")?;
                        info!("video_fec subscribed");
                        ack_channel = Some(ch);
                    }
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
                                flow.on_peer_connected();
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
                                    core.clone(),
                                    cursor_shared.clone(),
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
                            dog.start(now_ms());
                            dog_frames = core.frames_delivered();
                        }
                        FlowAction::Terminated { error_code } => {
                            info!(error_code, "ConnectionTerminated");
                            if let Some(p) = &peer {
                                let _ = p.close().await;
                            }
                            core.close();
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    if let Some(p) = &peer {
        let _ = p.close().await;
    }
    core.close();
    Ok(())
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
    core: Arc<RxCore>,
    cursor_shared: Arc<CursorShared>,
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
    peer.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        let _ = tx.try_send(LocalEvent::PeerState(s));
        done()
    }));

    let tx = ev_tx.clone();
    peer.on_ice_candidate(Box::new(move |cand: Option<RTCIceCandidate>| {
        if let Some(cand) = cand {
            match cand.to_json() {
                Ok(init) => {
                    let _ = tx.try_send(LocalEvent::LocalCandidate(RTCIceCandidateInit {
                        candidate: init.candidate,
                        sdp_mid: init.sdp_mid,
                        sdp_mline_index: init.sdp_mline_index,
                        username_fragment: init.username_fragment,
                    }));
                }
                Err(e) => warn!("ice candidate to_json failed: {e}"),
            }
        }
        done()
    }));

    let tx = ev_tx.clone();
    peer.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let label = dc.label().to_string();
        debug!("data channel from host: \"{label}\"");
        match label.as_str() {
            "video_fec" => {
                let tx = tx.clone();
                dc.on_message(Box::new(move |msg: DataChannelMessage| {
                    let _ = tx.try_send(LocalEvent::FecData(msg.data.to_vec()));
                    done()
                }));
            }
            "video_fec_ack" => {
                let tx = tx.clone();
                let dc2 = dc.clone();
                if dc.ready_state() == RTCDataChannelState::Open {
                    let _ = tx.try_send(LocalEvent::FecAckOpen(dc2));
                } else {
                    dc.on_open(Box::new(move || {
                        let _ = tx.try_send(LocalEvent::FecAckOpen(dc2.clone()));
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
                    let dc2 = dc.clone();
                    if dc.ready_state() == RTCDataChannelState::Open {
                        let _ = tx.try_send(LocalEvent::InputOpen(id, dc2));
                    } else {
                        dc.on_open(Box::new(move || {
                            let _ = tx.try_send(LocalEvent::InputOpen(id, dc2.clone()));
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
}
