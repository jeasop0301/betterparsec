//! Pure signaling flow state machine (m6-native-spike.md §F-1).
//!
//! Consumes parsed [`StreamServerMessage`]s plus local peer events and emits
//! the outgoing [`StreamClientMessage`] sequence. No I/O, no WebRTC — the
//! async session drives it; tests pin the message order contract:
//!
//! `Init` → (server `Setup`) → `SetTransport(WebRTC)` → answer/ICE relay →
//! (peer connected) → `StartStream` exactly once → (server
//! `ConnectionComplete` / `ConnectionTerminated`).

use common::api_bindings::{
    RtcIceCandidate, RtcIceServer, RtcSessionDescription, StreamClientMessage, StreamServerMessage,
    StreamSettings, StreamSignalingMessage, TransportType,
};

/// Instructions for the driving session.
#[derive(Debug)]
pub enum FlowAction {
    /// Serialise and send on the signaling WebSocket.
    Send(StreamClientMessage),
    /// Create the peer with these ICE servers (fires once, on `Setup`).
    CreatePeer(Vec<RtcIceServer>),
    /// Apply the remote offer and produce an answer.
    ApplyRemoteOffer(RtcSessionDescription),
    /// Add a remote ICE candidate (session buffers until the peer exists).
    AddRemoteCandidate(RtcIceCandidate),
    /// Server confirmed the moonlight session: decoder setup parameters.
    Complete(StreamParams),
    /// Server terminated the stream.
    Terminated { error_code: i32 },
}

/// Decoder/audio setup parameters from `ConnectionComplete`
/// (maps onto the native shim's DECODE_UNIT / audio init).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamParams {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio_sample_rate: u32,
    pub audio_channel_count: u32,
    pub audio_streams: u32,
    pub audio_coupled_streams: u32,
    pub audio_samples_per_frame: u32,
    pub audio_mapping: [u8; 8],
}

/// Immutable per-session request parameters.
#[derive(Debug, Clone)]
pub struct FlowConfig {
    pub host_id: u32,
    pub app_id: u32,
    /// Web client default: 3 (web/default_settings.ts).
    pub video_frame_queue_size: usize,
    /// Web client default: 20.
    pub audio_sample_queue_size: usize,
    pub bitrate_kbps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `VideoFormats` bit mask (StreamSupportedVideoCodecs).
    pub supported_codecs: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Init sent, waiting for Setup.
    AwaitSetup,
    /// SetTransport(WebRTC) sent, negotiating.
    Negotiating,
    /// StartStream sent, waiting for ConnectionComplete.
    Starting,
    /// ConnectionComplete received.
    Streaming,
    /// ConnectionTerminated received.
    Terminated,
}

pub struct SignalingFlow {
    config: FlowConfig,
    phase: Phase,
}

impl SignalingFlow {
    pub fn new(config: FlowConfig) -> Self {
        Self {
            config,
            phase: Phase::AwaitSetup,
        }
    }

    /// The first message to send as soon as the WS is open.
    pub fn init_message(&self) -> StreamClientMessage {
        StreamClientMessage::Init {
            host_id: self.config.host_id,
            app_id: self.config.app_id,
            video_frame_queue_size: self.config.video_frame_queue_size,
            audio_sample_queue_size: self.config.audio_sample_queue_size,
        }
    }

    /// Consume one server message.
    pub fn on_server_message(&mut self, msg: StreamServerMessage) -> Vec<FlowAction> {
        match msg {
            StreamServerMessage::Setup { ice_servers } => {
                if self.phase != Phase::AwaitSetup {
                    return Vec::new(); // duplicate Setup: ignore
                }
                self.phase = Phase::Negotiating;
                vec![
                    FlowAction::CreatePeer(ice_servers),
                    FlowAction::Send(StreamClientMessage::SetTransport(TransportType::WebRTC)),
                ]
            }
            StreamServerMessage::WebRtc(StreamSignalingMessage::Description(desc)) => {
                vec![FlowAction::ApplyRemoteOffer(desc)]
            }
            StreamServerMessage::WebRtc(StreamSignalingMessage::AddIceCandidate(cand)) => {
                vec![FlowAction::AddRemoteCandidate(cand)]
            }
            StreamServerMessage::ConnectionComplete {
                format,
                width,
                height,
                fps,
                audio_sample_rate,
                audio_channel_count,
                audio_streams,
                audio_coupled_streams,
                audio_samples_per_frame,
                audio_mapping,
                ..
            } => {
                self.phase = Phase::Streaming;
                vec![FlowAction::Complete(StreamParams {
                    format,
                    width,
                    height,
                    fps,
                    audio_sample_rate,
                    audio_channel_count,
                    audio_streams,
                    audio_coupled_streams,
                    audio_samples_per_frame,
                    audio_mapping,
                })]
            }
            StreamServerMessage::ConnectionTerminated { error_code } => {
                self.phase = Phase::Terminated;
                vec![FlowAction::Terminated { error_code }]
            }
            // Informational — no state change.
            StreamServerMessage::UpdateApp { .. } | StreamServerMessage::DebugLog { .. } => {
                Vec::new()
            }
        }
    }

    /// The local answer SDP is ready — relay it.
    pub fn on_local_answer(&self, desc: RtcSessionDescription) -> FlowAction {
        FlowAction::Send(StreamClientMessage::WebRtc(
            StreamSignalingMessage::Description(desc),
        ))
    }

    /// A local ICE candidate gathered — relay it.
    pub fn on_local_candidate(&self, cand: RtcIceCandidate) -> FlowAction {
        FlowAction::Send(StreamClientMessage::WebRtc(
            StreamSignalingMessage::AddIceCandidate(cand),
        ))
    }

    /// Peer transitioned to connected. Emits `StartStream` exactly once
    /// (reconnect/re-fire must not restart the moonlight session).
    pub fn on_peer_connected(&mut self) -> Option<FlowAction> {
        if self.phase != Phase::Negotiating {
            return None;
        }
        self.phase = Phase::Starting;
        Some(FlowAction::Send(StreamClientMessage::StartStream {
            settings: StreamSettings {
                bitrate_kbps: self.config.bitrate_kbps,
                width: self.config.width,
                height: self.config.height,
                fps: self.config.fps,
                play_audio_local: false,
                supported_codecs: self.config.supported_codecs,
                hdr: false,
            },
        }))
    }

    pub fn is_terminated(&self) -> bool {
        self.phase == Phase::Terminated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> FlowConfig {
        FlowConfig {
            host_id: 1,
            app_id: 2,
            video_frame_queue_size: 3,
            audio_sample_queue_size: 20,
            bitrate_kbps: 8000,
            width: 1920,
            height: 1080,
            fps: 60,
            supported_codecs: 0x1,
        }
    }

    fn setup_msg() -> StreamServerMessage {
        StreamServerMessage::Setup {
            ice_servers: vec![],
        }
    }

    #[test]
    fn init_message_carries_request_parameters() {
        let flow = SignalingFlow::new(config());
        match flow.init_message() {
            StreamClientMessage::Init {
                host_id,
                app_id,
                video_frame_queue_size,
                audio_sample_queue_size,
            } => {
                assert_eq!(host_id, 1);
                assert_eq!(app_id, 2);
                assert_eq!(video_frame_queue_size, 3);
                assert_eq!(audio_sample_queue_size, 20);
            }
            other => panic!("unexpected init message: {other:?}"),
        }
    }

    #[test]
    fn setup_creates_peer_then_selects_webrtc_transport() {
        let mut flow = SignalingFlow::new(config());
        let actions = flow.on_server_message(setup_msg());
        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], FlowAction::CreatePeer(_)));
        assert!(matches!(
            actions[1],
            FlowAction::Send(StreamClientMessage::SetTransport(TransportType::WebRTC))
        ));

        // Duplicate Setup is ignored (no second peer, no second SetTransport).
        assert!(flow.on_server_message(setup_msg()).is_empty());
    }

    #[test]
    fn start_stream_fires_exactly_once_and_only_after_setup() {
        let mut flow = SignalingFlow::new(config());
        assert!(
            flow.on_peer_connected().is_none(),
            "connected before Setup must not start the stream"
        );

        flow.on_server_message(setup_msg());
        let action = flow.on_peer_connected().expect("first connect starts");
        assert!(matches!(
            action,
            FlowAction::Send(StreamClientMessage::StartStream { .. })
        ));
        assert!(
            flow.on_peer_connected().is_none(),
            "reconnect must not restart the moonlight session"
        );
    }

    #[test]
    fn complete_and_terminated_reach_terminal_phases() {
        let mut flow = SignalingFlow::new(config());
        flow.on_server_message(setup_msg());
        flow.on_peer_connected();

        let actions = flow.on_server_message(StreamServerMessage::ConnectionComplete {
            capabilities: common::api_bindings::StreamCapabilities { touch: false },
            format: 0x1,
            width: 1920,
            height: 1080,
            fps: 60,
            audio_sample_rate: 48000,
            audio_channel_count: 2,
            audio_streams: 1,
            audio_coupled_streams: 1,
            audio_samples_per_frame: 240,
            audio_mapping: [0, 1, 0, 0, 0, 0, 0, 0],
        });
        let FlowAction::Complete(params) = &actions[0] else {
            panic!("expected Complete, got {actions:?}");
        };
        assert_eq!(params.width, 1920);
        assert_eq!(params.audio_samples_per_frame, 240);
        assert!(!flow.is_terminated());

        let actions =
            flow.on_server_message(StreamServerMessage::ConnectionTerminated { error_code: 7 });
        assert!(matches!(
            actions[0],
            FlowAction::Terminated { error_code: 7 }
        ));
        assert!(flow.is_terminated());
    }

    /// Wire-format pin: the JSON this flow emits must stay byte-compatible
    /// with what the web-server parses (externally tagged serde enums).
    #[test]
    fn client_message_json_shape_matches_server_contract() {
        let flow = SignalingFlow::new(config());
        let json = serde_json::to_value(flow.init_message()).expect("serialise");
        assert_eq!(json["Init"]["host_id"], 1);
        assert_eq!(json["Init"]["video_frame_queue_size"], 3);

        let set = serde_json::to_value(StreamClientMessage::SetTransport(TransportType::WebRTC))
            .expect("serialise");
        assert_eq!(set["SetTransport"], "WebRTC");
    }
}
