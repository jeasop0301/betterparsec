//! Async client session: login → WS signaling → WebRTC peer (answerer) →
//! `video_fec` subscribe → [`RxCore`] feed (m6-native-spike.md §F).
//!
//! Threading: [`Session::start`] spawns a dedicated tokio runtime thread;
//! the caller (C ABI or ct-probe) pulls frames through the shared
//! [`RxCore`] and stops via [`Session::stop`].

use std::future::ready;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use common::api_bindings::{
    RtcIceCandidate, RtcIceServer, RtcSdpType, RtcSessionDescription, StreamClientMessage,
    StreamServerMessage, StreamSignalingMessage,
};
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
use crate::flow::{FlowAction, FlowConfig, SignalingFlow, StreamParams};
use crate::tls::{ServerTrust, client_config};

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

pub struct Session {
    state: Arc<AtomicU8>,
    params: Arc<std::sync::Mutex<Option<StreamParams>>>,
    stop_tx: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Spawn the session on its own runtime thread. Frames/ACKs/needs-IDR
    /// flow through `core`; the caller keeps pulling from it as usual.
    pub fn start(config: SessionConfig, core: Arc<RxCore>) -> Self {
        let state = Arc::new(AtomicU8::new(SessionState::Connecting as u8));
        let params = Arc::new(std::sync::Mutex::new(None));
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);

        let state2 = state.clone();
        let params2 = params.clone();
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
                let result =
                    rt.block_on(run_session(config, core, state2.clone(), params2, stop_rx));
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
            thread: Some(thread),
        }
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

async fn run_session(
    config: SessionConfig,
    core: Arc<RxCore>,
    state: Arc<AtomicU8>,
    params_out: Arc<std::sync::Mutex<Option<StreamParams>>>,
    mut stop_rx: mpsc::Receiver<()>,
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

    send_ws(&ws_tx, &flow.init_message()).await?;

    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                                create_peer(&api, ice_servers, ev_tx.clone(), core.clone())
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
                            *params_out
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(params);
                            set_state(&state, SessionState::Streaming);
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
            _ => {
                // Control/input channels — not used by the W1 receive probe.
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
