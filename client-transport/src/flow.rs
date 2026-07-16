//! Pure signaling flow state machine (m6-native-spike.md §F-1).
//!
//! Consumes parsed [`StreamServerMessage`]s and emits the outgoing
//! [`StreamClientMessage`] sequence. No I/O, no WebRTC — the async session
//! drives it; tests pin the message order contract:
//!
//! `Init` → (server `Setup`) → `SetTransport(WebRTC)` → `StartStream` →
//! answer/ICE relay → (server `ConnectionComplete` / `ConnectionTerminated`).

use std::num::NonZeroU32;

use common::api_bindings::{
    RtcIceCandidate, RtcIceServer, RtcSessionDescription, StreamCapabilities, StreamClientMessage,
    StreamServerMessage, StreamSettings, StreamSignalingMessage, TransportType,
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
    /// A locally detected protocol violation. The session must fail locally;
    /// this is not a server-originated `ConnectionTerminated`.
    ProtocolError(FlowProtocolError),
    /// Server terminated the stream.
    Terminated { error_code: i32 },
}

/// A malformed selected FEC capability or invalid signaling order is a local
/// protocol error, never a request to silently downgrade the native decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowProtocolError {
    InvalidFecCapabilityTuple {
        selected_fec_protocol_version: Option<u8>,
        fec_epoch: Option<u32>,
    },
    ConnectionCompleteBeforeSetup,
    DuplicateConnectionComplete,
    ConnectionCompleteAfterTermination,
}

/// Validate the exact selected FEC capability tuple and return the version and
/// sender-owned epoch that must be latched for this stream.
pub fn negotiated_fec_parameters(
    capabilities: &StreamCapabilities,
) -> Result<(u8, Option<NonZeroU32>), FlowProtocolError> {
    match (
        capabilities.selected_fec_protocol_version,
        capabilities.fec_epoch,
    ) {
        (None, None) | (Some(1), None) => Ok((1, None)),
        (Some(2), Some(epoch)) if epoch != 0 => Ok((
            2,
            Some(NonZeroU32::new(epoch).expect("epoch checked nonzero")),
        )),
        (selected_fec_protocol_version, fec_epoch) => {
            Err(FlowProtocolError::InvalidFecCapabilityTuple {
                selected_fec_protocol_version,
                fec_epoch,
            })
        }
    }
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
    /// FEC wire version selected by the exact validated capability tuple.
    pub fec_protocol_version: u8,
    /// Sender-owned v2 epoch. This is absent for legacy v1 streams.
    pub fec_epoch: Option<NonZeroU32>,
}

/// Clamp an SDP-negotiated audio channel count into the decodable range.
///
/// `sdp_ch` is `StreamParams::audio_channel_count` off `ConnectionComplete`.
/// Valid negotiated counts are `1..=8`; `0` (not yet known / not sent) and
/// anything outside that range are treated as invalid and fall back to
/// stereo — the one layout every device path (WASAPI shared *and*
/// exclusive) is guaranteed to support, rather than clamping a bogus large
/// count down to 8 and pretending it was negotiated.
pub fn negotiated_channels(sdp_ch: u32) -> u16 {
    if (1..=8).contains(&sdp_ch) {
        sdp_ch as u16
    } else {
        2
    }
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
    /// SetTransport(WebRTC) + StartStream sent, negotiating.
    Negotiating,
    /// ConnectionComplete received.
    Streaming,
    /// Server termination or a locally detected protocol error.
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
                // Web-client parity (index.ts tryWebRTCTransport →
                // startStream): StartStream goes out immediately after
                // SetTransport — the streamer starts the moonlight session
                // and WebRTC negotiation in parallel, and only produces the
                // SDP offer once StartStream has arrived. Waiting for
                // peer-connected before StartStream deadlocks (live probe,
                // 2026-07-14).
                vec![
                    FlowAction::CreatePeer(ice_servers),
                    FlowAction::Send(StreamClientMessage::SetTransport(TransportType::WebRTC)),
                    FlowAction::Send(StreamClientMessage::StartStream {
                        settings: StreamSettings {
                            bitrate_kbps: self.config.bitrate_kbps,
                            width: self.config.width,
                            height: self.config.height,
                            fps: self.config.fps,
                            play_audio_local: false,
                            supported_codecs: self.config.supported_codecs,
                            hdr: false,
                            // Native video rides the video_fec DataChannel;
                            // tell the streamer to skip the duplicate
                            // RTP-track send (halves the session wire).
                            video_over_fec_only: true,
                            // Native client supports the exact v2 FEC wire
                            // format while still accepting a v1 selection.
                            requested_fec_protocol_version: Some(2),
                        },
                    }),
                ]
            }
            StreamServerMessage::WebRtc(StreamSignalingMessage::Description(desc)) => {
                vec![FlowAction::ApplyRemoteOffer(desc)]
            }
            StreamServerMessage::WebRtc(StreamSignalingMessage::AddIceCandidate(cand)) => {
                vec![FlowAction::AddRemoteCandidate(cand)]
            }
            StreamServerMessage::ConnectionComplete {
                capabilities,
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
            } => {
                let phase_error = match self.phase {
                    Phase::AwaitSetup => Some(FlowProtocolError::ConnectionCompleteBeforeSetup),
                    Phase::Streaming => Some(FlowProtocolError::DuplicateConnectionComplete),
                    Phase::Terminated => {
                        Some(FlowProtocolError::ConnectionCompleteAfterTermination)
                    }
                    Phase::Negotiating => None,
                };
                if let Some(error) = phase_error {
                    self.phase = Phase::Terminated;
                    return vec![FlowAction::ProtocolError(error)];
                }

                let (fec_protocol_version, fec_epoch) =
                    match negotiated_fec_parameters(&capabilities) {
                        Ok(parameters) => parameters,
                        Err(error) => {
                            self.phase = Phase::Terminated;
                            return vec![FlowAction::ProtocolError(error)];
                        }
                    };
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
                    fec_protocol_version,
                    fec_epoch,
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
    fn complete_msg(capabilities: StreamCapabilities) -> StreamServerMessage {
        StreamServerMessage::ConnectionComplete {
            capabilities,
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
        }
    }

    fn capabilities(
        selected_fec_protocol_version: Option<u8>,
        fec_epoch: Option<u32>,
    ) -> StreamCapabilities {
        StreamCapabilities {
            touch: false,
            selected_fec_protocol_version,
            fec_epoch,
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
    fn setup_creates_peer_selects_transport_and_starts_stream() {
        let mut flow = SignalingFlow::new(config());
        let actions = flow.on_server_message(setup_msg());
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[0], FlowAction::CreatePeer(_)));
        assert!(matches!(
            actions[1],
            FlowAction::Send(StreamClientMessage::SetTransport(TransportType::WebRTC))
        ));
        // Web-client parity: StartStream immediately after SetTransport —
        // the streamer only produces the SDP offer once StartStream arrived
        // (deadlock pin, live probe 2026-07-14).
        let FlowAction::Send(StreamClientMessage::StartStream { settings }) = &actions[2] else {
            panic!("expected StartStream third, got {actions:?}");
        };
        assert_eq!(settings.bitrate_kbps, 8000);
        assert_eq!(settings.supported_codecs, 0x1);
        assert_eq!(settings.requested_fec_protocol_version, Some(2));

        // Duplicate Setup is ignored (no second peer/transport/stream).
        assert!(flow.on_server_message(setup_msg()).is_empty());
    }

    #[test]
    fn complete_and_terminated_reach_terminal_phases() {
        let mut flow = SignalingFlow::new(config());
        flow.on_server_message(setup_msg());

        let actions = flow.on_server_message(complete_msg(capabilities(None, None)));
        let FlowAction::Complete(params) = &actions[0] else {
            panic!("expected Complete, got {actions:?}");
        };
        assert_eq!(params.width, 1920);
        assert_eq!(params.audio_samples_per_frame, 240);
        assert_eq!(params.fec_protocol_version, 1);
        assert_eq!(params.fec_epoch, None);
        assert!(!flow.is_terminated());

        let actions =
            flow.on_server_message(StreamServerMessage::ConnectionTerminated { error_code: 7 });
        assert!(matches!(
            actions[0],
            FlowAction::Terminated { error_code: 7 }
        ));
        assert!(flow.is_terminated());
    }

    #[test]
    fn fec_capability_tuples_are_validated_exactly() {
        for selected_fec_protocol_version in [None, Some(1), Some(2), Some(3)] {
            for fec_epoch in [None, Some(0), Some(42)] {
                let result = negotiated_fec_parameters(&capabilities(
                    selected_fec_protocol_version,
                    fec_epoch,
                ));
                match (selected_fec_protocol_version, fec_epoch) {
                    (None, None) | (Some(1), None) => assert_eq!(result, Ok((1, None))),
                    (Some(2), Some(42)) => {
                        assert_eq!(result, Ok((2, NonZeroU32::new(42))))
                    }
                    _ => assert_eq!(
                        result,
                        Err(FlowProtocolError::InvalidFecCapabilityTuple {
                            selected_fec_protocol_version,
                            fec_epoch,
                        })
                    ),
                }
            }
        }
    }

    #[test]
    fn complete_is_accepted_once_only_while_negotiating() {
        let mut before_setup = SignalingFlow::new(config());
        assert!(matches!(
            before_setup
                .on_server_message(complete_msg(capabilities(None, None)))
                .as_slice(),
            [FlowAction::ProtocolError(
                FlowProtocolError::ConnectionCompleteBeforeSetup
            )]
        ));
        assert!(before_setup.is_terminated());

        let mut flow = SignalingFlow::new(config());
        flow.on_server_message(setup_msg());
        assert!(matches!(
            flow.on_server_message(complete_msg(capabilities(Some(2), Some(42))))
                .as_slice(),
            [FlowAction::Complete(_)]
        ));
        assert!(matches!(
            flow.on_server_message(complete_msg(capabilities(Some(2), Some(42))))
                .as_slice(),
            [FlowAction::ProtocolError(
                FlowProtocolError::DuplicateConnectionComplete
            )]
        ));
        assert!(flow.is_terminated());

        let mut after_termination = SignalingFlow::new(config());
        after_termination
            .on_server_message(StreamServerMessage::ConnectionTerminated { error_code: 7 });
        assert!(matches!(
            after_termination
                .on_server_message(complete_msg(capabilities(None, None)))
                .as_slice(),
            [FlowAction::ProtocolError(
                FlowProtocolError::ConnectionCompleteAfterTermination
            )]
        ));
    }

    #[test]
    fn malformed_capabilities_produce_typed_local_protocol_errors() {
        let mut flow = SignalingFlow::new(config());
        flow.on_server_message(setup_msg());

        assert!(matches!(
            flow.on_server_message(complete_msg(capabilities(Some(2), None)))
                .as_slice(),
            [FlowAction::ProtocolError(
                FlowProtocolError::InvalidFecCapabilityTuple {
                    selected_fec_protocol_version: Some(2),
                    fec_epoch: None,
                }
            )]
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

    /// GATING acceptance (S2 surround N-channel decode): clamp behaviour
    /// plus the 0/invalid → stereo fallback.
    #[test]
    fn negotiated_channels_clamps_and_falls_back_to_stereo() {
        // 0 (not yet known / not negotiated) → stereo.
        assert_eq!(negotiated_channels(0), 2);
        // In-range counts pass through unchanged.
        assert_eq!(negotiated_channels(1), 1);
        assert_eq!(negotiated_channels(2), 2);
        assert_eq!(negotiated_channels(6), 6);
        assert_eq!(negotiated_channels(8), 8);
        // Out-of-range (invalid) → stereo, not clamped to 8.
        assert_eq!(negotiated_channels(9), 2);
        assert_eq!(negotiated_channels(100), 2);
    }
}
