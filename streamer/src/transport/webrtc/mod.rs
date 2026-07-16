use std::{
    future::ready,
    pin::Pin,
    sync::{Arc, Weak, atomic::AtomicU32},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use common::{
    api_bindings::{
        RtcIceCandidate, RtcSdpType, RtcSessionDescription, StreamClientMessage,
        StreamServerMessage, StreamSignalingMessage, TransportChannelId,
    },
    config::{PortRange, WebRtcConfig},
    ipc::{ServerIpcMessage, StreamerIpcMessage},
};
use moonlight_common::stream::{
    audio::{AudioConfig, OpusMultistreamConfig},
    video::{DecodeResult, VideoDecodeUnit, VideoFormats, VideoSetup},
};
use tokio::{
    runtime::Handle,
    spawn,
    sync::{
        Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    time::sleep,
};
use tracing::{debug, error, trace, warn};
use webrtc::{
    api::{
        APIBuilder, interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
        setting_engine::SettingEngine,
    },
    data_channel::{
        RTCDataChannel, data_channel_init::RTCDataChannelInit,
        data_channel_message::DataChannelMessage,
    },
    ice::udp_network::{EphemeralUDP, UDPNetwork},
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
        ice_connection_state::RTCIceConnectionState,
    },
    interceptor::registry::Registry,
    peer_connection::{
        RTCPeerConnection,
        configuration::RTCConfiguration,
        offer_answer_options::RTCOfferOptions,
        peer_connection_state::RTCPeerConnectionState,
        sdp::{sdp_type::RTCSdpType, session_description::RTCSessionDescription},
    },
};

use crate::{
    TIMEOUT_DURATION,
    cc::CcShared,
    convert::{
        from_webrtc_sdp, into_webrtc_ice, into_webrtc_ice_candidate, into_webrtc_network_type,
    },
    transport::{
        InboundPacket, OutboundPacket, TransportChannel, TransportError, TransportEvent,
        TransportEvents, TransportSender,
        metrics::{VideoTransportMetrics, VideoTransportStats},
        webrtc::{
            audio::{WebRtcAudio, register_audio_codecs},
            sender::register_header_extensions,
            video::{WebRtcVideo, register_video_codecs},
        },
    },
};

mod audio;
mod fec_sender;
// Wire/framing moved to the shared transport-core crate (M6 W1); re-exported
// here so `crate::transport::webrtc::fec_wire::*` call sites stay unchanged.
pub use transport_core::fec_wire;
mod qu_relay;
pub(crate) mod qu_wire;
mod sender;
mod video;

struct WebRtcInner {
    peer: Arc<RTCPeerConnection>,
    event_sender: Sender<TransportEvent>,
    general_channel: Arc<RTCDataChannel>,
    stats_channel: Arc<RTCDataChannel>,
    input_channels: Mutex<Vec<Arc<RTCDataChannel>>>,
    /// Unreliable, unordered DataChannel carrying FEC source + repair symbols
    /// from host to client (host → client only; no on_message handler needed).
    video_fec_channel: Arc<RTCDataChannel>,
    /// Reliable, ordered DataChannel carrying QU tile messages (bidirectional:
    /// host → client config/tile/invalidate, client → host subscribe/budget).
    video_qu_channel: Arc<RTCDataChannel>,
    /// Forwards decoded inbound CLIPBOARD text (client → host) to the
    /// clipboard watcher's apply path (transport::clipboard).
    clipboard_apply_tx: Sender<String>,
    video: Mutex<WebRtcVideo>,
    target_bitrate_kbps: Arc<AtomicU32>,
    cc_shared: Arc<CcShared>,
    video_metrics: Arc<VideoTransportMetrics>,
    audio: Mutex<WebRtcAudio>,
    // Timeout / Terminate
    pub timeout_terminate_request: Mutex<Option<Instant>>,
}

pub async fn new(
    config: &WebRtcConfig,
    video_frame_queue_size: usize,
    audio_sample_queue_size: usize,
) -> Result<(WebRTCTransportSender, WebRTCTransportEvents), anyhow::Error> {
    // -- Configure WebRTC
    let rtc_config = RTCConfiguration {
        ice_servers: config
            .ice_servers
            .clone()
            .into_iter()
            .map(into_webrtc_ice)
            .collect(),
        ..Default::default()
    };
    let mut api_settings = SettingEngine::default();

    if let Some(PortRange { min, max }) = config.port_range {
        match EphemeralUDP::new(min, max) {
            Ok(udp) => {
                api_settings.set_udp_network(UDPNetwork::Ephemeral(udp));
            }
            Err(err) => {
                warn!("[Stream]: Invalid port range in config: {err:?}");
            }
        }
    }
    if let Some(mapping) = config.nat_1to1.as_ref() {
        api_settings.set_nat_1to1_ips(
            mapping.ips.clone(),
            into_webrtc_ice_candidate(mapping.ice_candidate_type),
        );
    }
    api_settings.set_network_types(
        config
            .network_types
            .iter()
            .copied()
            .map(into_webrtc_network_type)
            .collect(),
    );

    api_settings.set_include_loopback_candidate(config.include_loopback_candidates);

    // -- Register media codecs
    // TODO: register them based on the sdp
    // P2 wire-in (deferred, S5): `negotiate_codecs` below is the pure
    // intersection helper for SDP-driven codec pruning; wiring it into the
    // MediaEngine registration here (and threading a parsed SDP offer into
    // this fn) is a later live A/B slice — see docs/design for the codec
    // negotiation contract. Registration below stays unconditional.
    let mut api_media = MediaEngine::default();
    register_audio_codecs(&mut api_media).expect("failed to register audio codecs");
    register_video_codecs(&mut api_media).expect("failed to register video codecs");
    register_header_extensions(&mut api_media).expect("failed to register header extensions");

    // -- Build Api
    let mut api_registry = Registry::new();

    // Use the default set of Interceptors
    api_registry = register_default_interceptors(api_registry, &mut api_media)
        .expect("failed to register webrtc default interceptors");

    let api = APIBuilder::new()
        .with_setting_engine(api_settings)
        .with_media_engine(api_media)
        .with_interceptor_registry(api_registry)
        .build();

    let (event_sender, event_receiver) = channel::<TransportEvent>(20);

    let peer = Arc::new(api.new_peer_connection(rtc_config).await?);

    let general_channel = peer.create_data_channel("general", None).await?;
    let stats_channel = peer.create_data_channel("stats", None).await?;

    // FEC symbol channel: unreliable (maxRetransmits=0), unordered — UDP semantics.
    let video_fec_channel = peer
        .create_data_channel(
            "video_fec",
            Some(RTCDataChannelInit {
                ordered: Some(false),
                max_retransmits: Some(0),
                ..Default::default()
            }),
        )
        .await?;
    // FEC ACK channel: reliable, ordered — window ACKs from client to host.
    let video_fec_ack_channel = peer
        .create_data_channel(
            "video_fec_ack",
            Some(RTCDataChannelInit {
                ordered: Some(true),
                ..Default::default()
            }),
        )
        .await?;
    // QU tile channel: reliable, ordered — host→client tiles + client→host sub/budget.
    let video_qu_channel = peer
        .create_data_channel(
            "video_qu",
            Some(RTCDataChannelInit {
                ordered: Some(true),
                ..Default::default()
            }),
        )
        .await?;
    // Cursor visibility channel: reliable, ordered — host-authority POS
    // messages for the client's auto mouse-mode (cursor-channel.md §3 P1).
    let cursor_channel = peer
        .create_data_channel(
            "cursor",
            Some(RTCDataChannelInit {
                ordered: Some(true),
                ..Default::default()
            }),
        )
        .await?;
    crate::transport::cursor_tracker::spawn(crate::transport::cursor_tracker::WebRtcCursorSink(
        cursor_channel,
    ));
    // Clipboard channel: reliable, ordered — bidirectional text sync
    // (clipboard.rs; research/05 gap vs Parsec/DCV).
    let clipboard_channel = peer
        .create_data_channel(
            "clipboard",
            Some(RTCDataChannelInit {
                ordered: Some(true),
                ..Default::default()
            }),
        )
        .await?;
    let (clipboard_apply_tx, clipboard_apply_rx) = channel::<String>(20);
    crate::transport::clipboard::spawn(
        crate::transport::clipboard::WebRtcClipboardSink(clipboard_channel.clone()),
        clipboard_apply_rx,
    );

    let runtime = Handle::current();
    let video_metrics = Arc::new(VideoTransportMetrics::new(video_frame_queue_size));
    let target_bitrate_kbps = Arc::new(AtomicU32::new(0));
    let cc_shared = Arc::new(CcShared::new());
    let this_owned = Arc::new(WebRtcInner {
        peer: peer.clone(),
        event_sender,
        general_channel: general_channel.clone(),
        stats_channel,
        input_channels: Default::default(),
        video_fec_channel: video_fec_channel.clone(),
        video_qu_channel: video_qu_channel.clone(),
        clipboard_apply_tx,
        video: Mutex::new(WebRtcVideo::new(
            runtime.clone(),
            Arc::downgrade(&peer),
            video_frame_queue_size,
            video_metrics.clone(),
            target_bitrate_kbps.clone(),
            cc_shared.clone(),
        )),
        target_bitrate_kbps,
        cc_shared,
        video_metrics,
        audio: Mutex::new(WebRtcAudio::new(
            runtime,
            Arc::downgrade(&peer),
            audio_sample_queue_size,
        )),
        timeout_terminate_request: Mutex::new(None),
    });

    // Add all data channels. The server creates all data channels
    {
        let this = this_owned.clone();
        this.clone().on_data_channel(general_channel).await;
        // FEC symbol channel (host→client; no on_message handler needed on host).
        this.clone().on_data_channel(video_fec_channel).await;
        // FEC ACK channel (client→host; on_message set up in on_data_channel).
        this.clone().on_data_channel(video_fec_ack_channel).await;
        // QU tile channel (bidirectional; on_message set up in on_data_channel).
        this.clone().on_data_channel(video_qu_channel).await;
        // Clipboard channel (bidirectional; on_message set up in on_data_channel).
        this.clone().on_data_channel(clipboard_channel).await;

        struct Options {
            reliable: bool,
            ordered: bool,
        }
        #[rustfmt::skip]
        const INPUT_CHANNELS: &[(&str, Options)] = &[
            ("mouse_reliable", Options { reliable: true , ordered: true  }),
            ("mouse_absolute", Options { reliable: false, ordered: false }),
            ("mouse_relative", Options { reliable: true , ordered: false }),
            ("keyboard",       Options { reliable: true , ordered: true  }),
            ("touch",          Options { reliable: true , ordered: true  }),
            ("controllers",    Options { reliable: true , ordered: true  }),
            ("controller0",    Options { reliable: false, ordered: false }),
            ("controller1",    Options { reliable: false, ordered: false }),
            ("controller2",    Options { reliable: false, ordered: false }),
            ("controller3",    Options { reliable: false, ordered: false }),
            ("controller4",    Options { reliable: false, ordered: false }),
            ("controller5",    Options { reliable: false, ordered: false }),
            ("controller6",    Options { reliable: false, ordered: false }),
            ("controller7",    Options { reliable: false, ordered: false }),
            ("controller8",    Options { reliable: false, ordered: false }),
            ("controller9",    Options { reliable: false, ordered: false }),
            ("controller10",   Options { reliable: false, ordered: false }),
            ("controller11",   Options { reliable: false, ordered: false }),
            ("controller12",   Options { reliable: false, ordered: false }),
            ("controller13",   Options { reliable: false, ordered: false }),
            ("controller14",   Options { reliable: false, ordered: false }),
            ("controller15",   Options { reliable: false, ordered: false }),
        ];

        let mut input_channels = this.input_channels.lock().await;
        for (channel, options) in INPUT_CHANNELS {
            let data_channel = this
                .peer
                .create_data_channel(
                    channel,
                    Some(RTCDataChannelInit {
                        ordered: Some(options.ordered),
                        max_retransmits: (!options.reliable).then_some(0),
                        ..Default::default()
                    }),
                )
                .await?;

            this.clone().on_data_channel(data_channel.clone()).await;

            input_channels.push(data_channel);
        }
    }

    let this = Arc::downgrade(&this_owned);

    // -- Connection state
    peer.on_ice_connection_state_change(create_event_handler(
        this.clone(),
        async move |this, state| {
            this.on_ice_connection_state_change(state).await;
        },
    ));
    peer.on_peer_connection_state_change(create_event_handler(
        this.clone(),
        async move |this, state| {
            this.on_peer_connection_state_change(state).await;
        },
    ));

    // -- Signaling
    peer.on_ice_candidate(create_event_handler(
        this.clone(),
        async move |this, candidate| {
            this.on_ice_candidate(candidate).await;
        },
    ));

    drop(peer);

    Ok((
        WebRTCTransportSender {
            inner: this_owned.clone(),
        },
        WebRTCTransportEvents { event_receiver },
    ))
}

// It compiling...
#[allow(clippy::complexity)]
fn create_event_handler<F, Args>(
    inner: Weak<WebRtcInner>,
    f: F,
) -> Box<
    dyn FnMut(Args) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync + 'static,
>
where
    Args: Send + 'static,
    F: AsyncFn(Arc<WebRtcInner>, Args) + Send + Sync + Clone + 'static,
    for<'a> F::CallRefFuture<'a>: Send,
{
    Box::new(move |args: Args| {
        let inner = inner.clone();
        let Some(inner) = inner.upgrade() else {
            debug!("Called webrtc event handler while the main type is already deallocated");
            return Box::pin(ready(())) as Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
        };

        let future = f.clone();
        Box::pin(async move {
            future(inner, args).await;
        }) as Pin<Box<dyn Future<Output = ()> + Send + 'static>>
    })
        as Box<
            dyn FnMut(Args) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync
                + 'static,
        >
}
#[allow(clippy::complexity)]
fn create_channel_message_handler(
    inner: Weak<WebRtcInner>,
    channel: TransportChannel,
) -> Box<
    dyn FnMut(DataChannelMessage) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
        + Send
        + Sync
        + 'static,
> {
    debug!("setting up channel {:?}", channel);
    create_event_handler(inner, async move |inner, message: DataChannelMessage| {
        let Some(packet) = InboundPacket::deserialize(channel, &message.data) else {
            return;
        };

        if let Err(err) = inner
            .event_sender
            .send(TransportEvent::RecvPacket(packet))
            .await
        {
            warn!("Failed to dispatch RecvPacket event: {err:?}");
        };
    })
}

impl WebRtcInner {
    // -- Handle Connection State
    async fn on_ice_connection_state_change(self: &Arc<Self>, _state: RTCIceConnectionState) {}
    async fn on_peer_connection_state_change(self: Arc<Self>, state: RTCPeerConnectionState) {
        #[allow(clippy::collapsible_if)]
        if matches!(state, RTCPeerConnectionState::Closed) {
            if let Err(err) = self.event_sender.send(TransportEvent::Closed).await {
                warn!("Failed to send peer closed event to stream: {err:?}");
                self.request_terminate().await;
            };
        } else if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected
        ) {
            self.request_terminate().await;
        } else {
            self.clear_terminate_request().await;
        }
    }

    // -- Handle Signaling
    /// `ice_restart = true` creates the offer with fresh ICE credentials
    /// (M4 stall watchdog `RestartIce` rung); the client answers the re-offer
    /// through the normal signaling path.
    async fn send_offer(&self, ice_restart: bool) -> bool {
        let offer_options = ice_restart.then(|| RTCOfferOptions {
            ice_restart: true,
            ..Default::default()
        });
        let local_description = match self.peer.create_offer(offer_options).await {
            Err(err) => {
                error!("[Signaling]: failed to create offer: {err:?}");
                return false;
            }
            Ok(value) => value,
        };

        if let Err(err) = self
            .peer
            .set_local_description(local_description.clone())
            .await
        {
            error!("[Signaling]: failed to set local description: {err:?}");
            return false;
        }

        debug!(
            "[Signaling] Sending Local Description as Offer: {:?}",
            local_description.sdp
        );

        if let Err(err) = self
            .event_sender
            .send(TransportEvent::SendIpc(StreamerIpcMessage::WebSocket(
                StreamServerMessage::WebRtc(StreamSignalingMessage::Description(
                    RtcSessionDescription {
                        ty: from_webrtc_sdp(local_description.sdp_type),
                        sdp: local_description.sdp,
                    },
                )),
            )))
            .await
        {
            warn!("Failed to send local description (offer) via web socket from peer: {err:?}");
        };

        true
    }

    async fn on_ws_message(&self, message: StreamClientMessage) {
        match message {
            StreamClientMessage::StartStream { settings } => {
                let video_supported_formats = VideoFormats::from_bits(settings.supported_codecs)
                    .unwrap_or_else(|| {
                        warn!(
                            "Failed to deserialize VideoFormats: {}, falling back to only H264",
                            VideoFormats::from_bits_retain(settings.supported_codecs)
                        );
                        VideoFormats::H264
                    });
                {
                    let mut video = self.video.lock().await;
                    video.set_codecs(video_supported_formats).await;
                    // FEC-primary client (native): skip the duplicate
                    // RTP-track send once FEC carries frames (video.rs).
                    video.set_video_over_fec_only(settings.video_over_fec_only);
                    // Feature #1: seed the ABR ceiling from the initial bitrate.
                    video.set_configured_bitrate_kbps(settings.bitrate_kbps);
                }

                // TODO: check peer for supported formats via sdp

                if let Err(err) = self
                    .event_sender
                    .send(TransportEvent::StartStream { settings })
                    .await
                {
                    error!("Failed to send start stream: {err}");
                }
            }
            StreamClientMessage::WebRtc(StreamSignalingMessage::Description(description)) => {
                debug!("[Signaling] Received Remote Description: {:?}", description);

                let description = match &description.ty {
                    RtcSdpType::Offer => RTCSessionDescription::offer(description.sdp),
                    RtcSdpType::Answer => RTCSessionDescription::answer(description.sdp),
                    RtcSdpType::Pranswer => RTCSessionDescription::pranswer(description.sdp),
                    _ => {
                        error!(
                            "[Signaling]: failed to handle RTCSdpType {:?}",
                            description.ty
                        );
                        return;
                    }
                };

                let Ok(description) = description else {
                    error!("[Signaling]: Received invalid RTCSessionDescription");
                    return;
                };

                let remote_ty = description.sdp_type;

                if remote_ty == RTCSdpType::Offer {
                    warn!(
                        "Received an offer from the client. This shouldn't be possible. Dropping the offer"
                    );
                    return;
                }
                if let Err(err) = self.peer.set_remote_description(description).await {
                    error!("[Signaling]: failed to set remote description: {err:?}");
                }
            }
            StreamClientMessage::WebRtc(StreamSignalingMessage::AddIceCandidate(description)) => {
                debug!("[Signaling] Received Ice Candidate");

                if let Err(err) = self
                    .peer
                    .add_ice_candidate(RTCIceCandidateInit {
                        candidate: description.candidate,
                        sdp_mid: description.sdp_mid,
                        sdp_mline_index: description.sdp_mline_index,
                        username_fragment: description.username_fragment,
                    })
                    .await
                {
                    warn!("[Signaling]: failed to add ice candidate: {err:?}");
                }
            }
            // M4 stall watchdog: the client detected a stall and asks for an
            // IDR over the signaling socket (works even when the data path is
            // dead — PLI would need working RTCP).
            StreamClientMessage::RequestIdr => {
                debug!("[Watchdog]: client requested IDR via signaling");
                self.video.lock().await.request_idr();
            }
            // M4 stall watchdog: ICE restart rung — re-offer with fresh ICE
            // credentials so both sides re-gather candidate pairs.
            StreamClientMessage::RestartIce => {
                debug!("[Watchdog]: client requested ICE restart");
                if !self.send_offer(true).await {
                    warn!("[Watchdog]: failed to send ICE-restart offer");
                }
            }
            _ => {}
        }
    }

    async fn on_ice_candidate(&self, candidate: Option<RTCIceCandidate>) {
        let Some(candidate) = candidate else {
            return;
        };

        let Ok(candidate_json) = candidate.to_json() else {
            return;
        };

        debug!(
            "[Signaling] Sending Ice Candidate: {}",
            candidate_json.candidate
        );

        let message =
            StreamServerMessage::WebRtc(StreamSignalingMessage::AddIceCandidate(RtcIceCandidate {
                candidate: candidate_json.candidate,
                sdp_mid: candidate_json.sdp_mid,
                sdp_mline_index: candidate_json.sdp_mline_index,
                username_fragment: candidate_json.username_fragment,
            }));

        if let Err(err) = self
            .event_sender
            .send(TransportEvent::SendIpc(StreamerIpcMessage::WebSocket(
                message,
            )))
            .await
        {
            error!("Failed to send web socket message from peer: {err:?}");
        };
    }

    async fn on_data_channel(self: Arc<Self>, channel: Arc<RTCDataChannel>) {
        let label = channel.label();
        debug!("adding data channel: \"{label}\"");

        let inner = Arc::downgrade(&self);

        match label {
            "general" => {
                debug!("setting up general channel message handler");
                channel.on_message(create_channel_message_handler(
                    inner,
                    TransportChannel(TransportChannelId::GENERAL),
                ));
            }
            "mouse_reliable" | "mouse_absolute" | "mouse_relative" => {
                channel.on_message(create_channel_message_handler(
                    inner,
                    TransportChannel(TransportChannelId::MOUSE_ABSOLUTE),
                ));
            }
            "touch" => {
                channel.on_message(create_channel_message_handler(
                    inner,
                    TransportChannel(TransportChannelId::TOUCH),
                ));
            }
            "keyboard" => {
                channel.on_message(create_channel_message_handler(
                    inner,
                    TransportChannel(TransportChannelId::KEYBOARD),
                ));
            }
            "controllers" => {
                channel.on_message(create_channel_message_handler(
                    inner,
                    TransportChannel(TransportChannelId::CONTROLLERS),
                ));
            }
            "video_fec" => {
                // Host → client only; the host only sends on this channel.
                // No on_message handler required on the host side.
                debug!("video_fec channel open (host-to-client FEC symbols)");
            }
            "video_fec_ack" => {
                // Client → host ACK messages: Subscribe / NeedsIdr / Ack(seq).
                channel.on_message(create_event_handler(
                    inner,
                    async move |inner, msg: DataChannelMessage| {
                        let Some(ack_msg) = fec_wire::parse_ack_msg(&msg.data) else {
                            return;
                        };
                        inner.video.lock().await.handle_fec_ack(ack_msg);
                    },
                ));
            }
            "video_qu" => {
                // Bidirectional QU channel: forward client messages to the relay handle.
                // (host→client tile messages are sent directly on `video_qu_channel` by
                // the relay task; this handler only processes the client→host direction.)
                channel.on_message(create_event_handler(
                    inner,
                    async move |inner, msg: DataChannelMessage| {
                        inner.video.lock().await.handle_qu_msg(msg.data);
                    },
                ));
            }
            "clipboard" => {
                // Bidirectional clipboard channel: decode inbound (client → host)
                // text and forward it to the watcher's apply path. Outbound
                // (host → client) publishes are sent directly on
                // `clipboard_channel` by the clipboard watcher task.
                channel.on_message(create_event_handler(
                    inner,
                    async move |inner, msg: DataChannelMessage| {
                        let Some(text) = crate::transport::clipboard::decode_text(&msg.data) else {
                            return;
                        };
                        if inner.clipboard_apply_tx.send(text).await.is_err() {
                            debug!("[Clipboard] apply channel closed — dropping inbound text");
                        }
                    },
                ));
            }
            _ => {
                if let Some(number) = label.strip_prefix("controller")
                    && let Ok(id) = number.parse::<usize>()
                    && id < InboundPacket::CONTROLLER_CHANNELS.len()
                {
                    channel.on_message(create_channel_message_handler(
                        inner,
                        TransportChannel(InboundPacket::CONTROLLER_CHANNELS[id]),
                    ));
                }
            }
        };
    }

    // -- Termination
    async fn request_terminate(self: &Arc<Self>) {
        let this = self.clone();

        let mut terminate_request = self.timeout_terminate_request.lock().await;
        *terminate_request = Some(Instant::now());
        drop(terminate_request);

        spawn(async move {
            sleep(TIMEOUT_DURATION + Duration::from_millis(200)).await;

            let now = Instant::now();

            let terminate_request = this.timeout_terminate_request.lock().await;
            if let Some(terminate_request) = *terminate_request
                && (now - terminate_request) > TIMEOUT_DURATION
                && let Err(err) = this.event_sender.send(TransportEvent::Closed).await
            {
                warn!("Failed to send that the peer is closed: {err:?}");
            };
        });
    }
    async fn clear_terminate_request(&self) {
        let mut request = self.timeout_terminate_request.lock().await;

        *request = None;
    }
}

pub struct WebRTCTransportEvents {
    event_receiver: Receiver<TransportEvent>,
}

#[async_trait]
impl TransportEvents for WebRTCTransportEvents {
    async fn poll_event(&mut self) -> Result<TransportEvent, TransportError> {
        trace!("Polling WebRTCEvents");
        self.event_receiver
            .recv()
            .await
            .ok_or(TransportError::Closed)
    }
}

pub struct WebRTCTransportSender {
    inner: Arc<WebRtcInner>,
}

#[async_trait]
impl TransportSender for WebRTCTransportSender {
    async fn setup_video(&self, setup: VideoSetup) -> i32 {
        let mut video = self.inner.video.lock().await;
        if video.setup(&self.inner, setup).await {
            0
        } else {
            -1
        }
    }
    async fn send_video_unit<'a>(
        &'a self,
        unit: VideoDecodeUnit<&'a [u8]>,
    ) -> Result<DecodeResult, TransportError> {
        let mut video = self.inner.video.lock().await;
        Ok(video.send_decode_unit(&unit).await)
    }

    fn take_video_transport_stats(&self) -> Option<VideoTransportStats> {
        Some(self.inner.video_metrics.take_snapshot())
    }

    fn runtime_bitrate_target_kbps(&self) -> Option<Arc<AtomicU32>> {
        Some(self.inner.target_bitrate_kbps.clone())
    }

    fn runtime_cc_shared(&self) -> Option<Arc<CcShared>> {
        Some(self.inner.cc_shared.clone())
    }

    async fn setup_audio(
        &self,
        audio_config: AudioConfig,
        stream_config: OpusMultistreamConfig,
    ) -> i32 {
        let mut audio = self.inner.audio.lock().await;

        audio.setup(&self.inner, audio_config, stream_config).await
    }
    async fn send_audio_sample(&self, data: &[u8]) -> Result<(), TransportError> {
        let mut audio = self.inner.audio.lock().await;

        audio.send_audio_sample(data).await;

        Ok(())
    }

    async fn on_setup_complete(&self) {
        if !self.inner.send_offer(false).await {
            error!("Failed to send offer to client. Requesting Termination");
            self.inner.request_terminate().await;
        }
    }

    async fn send(&self, packet: OutboundPacket) -> Result<(), TransportError> {
        let mut buffer = Vec::new();

        let Some((channel, range)) = packet.serialize(&mut buffer) else {
            warn!("Failed to serialize packet: {packet:?}");
            return Ok(());
        };

        let bytes = Bytes::from(buffer);
        let bytes = bytes.slice(range);

        match channel.0 {
            TransportChannelId::GENERAL => match self.inner.general_channel.send(&bytes).await {
                Ok(_) => {}
                Err(webrtc::Error::ErrDataChannelNotOpen) => {
                    return Err(TransportError::ChannelClosed);
                }
                _ => {}
            },
            TransportChannelId::STATS => {
                if let Err(err) = self.inner.stats_channel.send(&bytes).await {
                    debug!(error = ?err, "Failed to send stat message");
                }
            }
            _ => {
                warn!("Cannot send data on channel {channel:?}");
                return Err(TransportError::ChannelClosed);
            }
        }
        Ok(())
    }

    async fn on_ipc_message(&self, message: ServerIpcMessage) -> Result<(), TransportError> {
        if let ServerIpcMessage::WebSocket(message) = message {
            self.inner.on_ws_message(message).await;
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.inner
            .peer
            .close()
            .await
            .map_err(|err| TransportError::Implementation(err.into()))?;

        Ok(())
    }
}

/// Pure codec-set negotiation: given the host's supported codec identifiers
/// (mime types, in host-priority order) and the set the peer declared
/// negotiable (e.g. parsed from an SDP offer's `a=rtpmap`/`a=fmtp` lines),
/// return the negotiated list, preserving host-priority order.
///
/// OUT OF SCOPE (P2 wire-in, see comment above `register_video_codecs`
/// call in [`new`]): this fn is not yet wired into the unconditional
/// `register_audio_codecs`/`register_video_codecs` MediaEngine registration.
/// It's a standalone building block for a later SDP-gated negotiation slice.
#[allow(dead_code)]
fn negotiate_codecs<'a>(host_supported: &[&'a str], peer_offered: &[&str]) -> Vec<&'a str> {
    host_supported
        .iter()
        .copied()
        .filter(|host_codec| {
            peer_offered
                .iter()
                .any(|peer_codec| peer_codec.eq_ignore_ascii_case(host_codec))
        })
        .collect()
}

#[cfg(test)]
mod codec_select_tests {
    use super::negotiate_codecs;

    #[test]
    fn register_codecs_from_sdp_selects_negotiated() {
        let host_supported = ["video/H265", "video/AV1", "video/H264"];
        let peer_offered = ["video/av1", "video/h264"];

        let negotiated = negotiate_codecs(&host_supported, &peer_offered);

        // Host priority order preserved; H265 dropped (not peer-offered).
        assert_eq!(negotiated, vec!["video/AV1", "video/H264"]);
    }

    #[test]
    fn negotiate_codecs_empty_intersection() {
        let host_supported = ["video/H265"];
        let peer_offered = ["video/VP9"];

        assert!(negotiate_codecs(&host_supported, &peer_offered).is_empty());
    }
}
