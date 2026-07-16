use std::{
    io::Cursor,
    ops::Range,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use bytes::{Bytes, BytesMut};
use common::{
    api_bindings::{LogMessageType, StreamServerMessage},
    ipc::StreamerIpcMessage,
};
use moonlight_common::stream::video::{
    DecodeResult, FrameType, VideoDecodeUnit, VideoFormat, VideoFormats, VideoSetup,
};
use tokio::runtime::Handle;
use tracing::{debug, error, info, trace, warn};
use webrtc::{
    api::media_engine::{MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_HEVC, MediaEngine},
    peer_connection::RTCPeerConnection,
    rtcp::{
        payload_feedbacks::{
            picture_loss_indication::PictureLossIndication,
            receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate,
        },
        receiver_report::ReceiverReport,
    },
    rtp::{
        codecs::{av1::Av1Payloader, h265::RTP_OUTBOUND_MTU},
        header::Header,
        packet::Packet,
        packetizer::Payloader,
    },
    rtp_transceiver::{
        RTCPFeedback,
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    },
    track::track_local::track_local_static_rtp::TrackLocalStaticRTP,
};

use crate::abr::{AbrConfig, AbrController};
use crate::cc::{self, CcConfig, CcController, CcShared};
use crate::transport::{
    TransportEvent,
    metrics::VideoTransportMetrics,
    webrtc::{
        WebRtcInner,
        fec_sender::FecSenderHandle,
        fec_wire::AckMsg,
        qu_relay::{DataChannelSink, QuRelayHandle},
        sender::{CcContext, SequencedTrackLocalStaticRTP, TrackLocalSender},
        video::{
            h264::{payloader::H264Payloader, reader::H264Reader},
            h265::{payloader::H265Payloader, reader::H265Reader},
        },
    },
};

mod annexb;
mod h264;
mod h265;

// Av1 specification:
// - https://aomediacodec.github.io/av1-rtp-spec/v1.0.0.html

enum VideoCodec {
    H264 {
        nal_reader: H264Reader<Cursor<Vec<u8>>>,
        payloader: H264Payloader,
    },
    H265 {
        nal_reader: H265Reader<Cursor<Vec<u8>>>,
        payloader: H265Payloader,
    },
    Av1 {
        payloader: Av1Payloader,
    },
}

pub struct WebRtcVideo {
    supported_video_formats: VideoFormats,
    sender: TrackLocalSender<SequencedTrackLocalStaticRTP>,
    needs_idr: Arc<AtomicBool>,
    clock_rate: u32,
    codec: Option<VideoCodec>,
    samples: Vec<BytesMut>,
    /// Configured stream bitrate (kbps) = the ABR ceiling. 0 until StartStream
    /// sets it; 0 disables adaptation (no ceiling to adapt within).
    configured_bitrate_kbps: u32,
    /// Live adaptive-bitrate target (kbps), driven by REMB in the RTCP loop.
    /// Consumed by the encoder-apply path (feature #1 path A, pending Sunshine).
    target_bitrate_kbps: Arc<AtomicU32>,
    /// Frame-delay CC cross-task state: target published by the sender loop,
    /// RR loss mailbox, frame interval. Composed with the ABR target by the
    /// runtime bitrate task (see `crate::cc::effective_target_kbps`).
    cc_shared: Arc<CcShared>,
    /// Monotonic generation counter shared with every spawned FEC sender task.
    /// Incremented in `setup()` to retire any still-running sender from a prior
    /// stream generation (ghost-writer guard, fec-framing.md §7 item 1).
    fec_generation: Arc<AtomicU32>,
    /// FEC sender handle; `None` until the first `setup()` call.
    /// Default state: dormant — one AtomicBool load per frame overhead only.
    fec_handle: Option<FecSenderHandle>,
    /// Monotonic generation counter shared with every spawned QU relay task.
    /// Same ghost-writer guard pattern as `fec_generation`.
    qu_generation: Arc<AtomicU32>,
    /// QU relay handle; `None` until the first `setup()` call.
    /// Dormant cost: idle TcpListener + epoch tracking only (no channel traffic).
    qu_handle: Option<QuRelayHandle>,
    /// FEC-primary client (StartStream `video_over_fec_only`): video rides
    /// the `video_fec` DataChannel, so the duplicate RTP-track send is
    /// skipped once FEC is actually carrying frames (see
    /// [`skip_rtp_track`]). Default false = legacy double-send.
    video_over_fec_only: bool,
}

impl WebRtcVideo {
    pub fn new(
        runtime: Handle,
        peer: Weak<RTCPeerConnection>,
        frame_queue_size: usize,
        metrics: Arc<VideoTransportMetrics>,
        target_bitrate_kbps: Arc<AtomicU32>,
        cc_shared: Arc<CcShared>,
    ) -> Self {
        Self {
            clock_rate: 0,
            needs_idr: Default::default(),
            sender: TrackLocalSender::new_with_metrics(runtime, peer, frame_queue_size, metrics),
            codec: None,
            supported_video_formats: VideoFormats::empty(),
            samples: Default::default(),
            configured_bitrate_kbps: 0,
            target_bitrate_kbps,
            cc_shared,
            fec_generation: Arc::new(AtomicU32::new(0)),
            fec_handle: None,
            qu_generation: Arc::new(AtomicU32::new(0)),
            qu_handle: None,
            video_over_fec_only: false,
        }
    }

    pub async fn set_codecs(&mut self, supported_codecs: VideoFormats) {
        self.supported_video_formats = supported_codecs;
    }

    /// StartStream `video_over_fec_only` (FEC-primary client — the native
    /// app). Set before [`Self::setup`], like `set_configured_bitrate_kbps`.
    pub fn set_video_over_fec_only(&mut self, on: bool) {
        self.video_over_fec_only = on;
    }

    /// M4 stall watchdog: the client asked for an IDR over the signaling
    /// socket. Same flag the RTCP PLI handler sets — consumed by the next
    /// `send_video_unit` poll.
    pub fn request_idr(&self) {
        self.needs_idr.store(true, Ordering::Release);
    }

    /// Sets the ABR ceiling from the stream's initial bitrate (kbps), and seeds
    /// the live target to it. Called at StartStream, before [`Self::setup`].
    pub fn set_configured_bitrate_kbps(&mut self, kbps: u32) {
        self.configured_bitrate_kbps = kbps;
        self.target_bitrate_kbps.store(kbps, Ordering::Release);
    }

    /// Shared handle to the live adaptive target (kbps). Consumed by the
    /// encoder-apply path (feature #1 path A, pending a Sunshine host patch —
    /// see docs/ROADMAP.md M2).
    pub async fn setup(
        &mut self,
        inner: &Arc<WebRtcInner>,
        VideoSetup {
            format,
            width,
            height,
            redraw_rate,
        }: VideoSetup,
    ) -> bool {
        info!("[Stream] Stream setup: {width}x{height}x{redraw_rate} and {format:?}");

        if !format.contained_in(self.supported_video_formats) {
            let message = format!(
                "The host tried to setup a video stream with a non supported video format: {format:?}, supported formats: {}",
                self.supported_video_formats
            );

            error!("{}", message);

            if let Err(err) = inner
                .event_sender
                .send(TransportEvent::SendIpc(StreamerIpcMessage::WebSocket(
                    StreamServerMessage::DebugLog {
                        message,
                        ty: Some(LogMessageType::FatalDescription),
                    },
                )))
                .await
            {
                warn!("Failed to send error to client: {err}");
            }

            return false;
        }

        let Some(codec) = video_format_to_codec(format) else {
            // This shouldn't happen
            error!("Failed to get video codec with format {:?}", format);
            return false;
        };

        // Frame-delay CC shares ABR's enable gate (a configured ceiling) and
        // its ceiling. begin_generation() runs unconditionally: it resets the
        // published target, discards unread loss, and — critically — turns
        // any still-running sender task from a previous setup into a ghost
        // whose CcShared writes read as inactive (create_track spawns a new
        // sample_sender without stopping the old one).
        self.cc_shared
            .set_frame_interval_us(cc::interval_us_from_fps(redraw_rate));
        let cc_generation = self.cc_shared.begin_generation();
        if self.configured_bitrate_kbps > 0 {
            self.sender.set_congestion_controller(CcContext::new(
                CcController::new(CcConfig::from_ceiling(self.configured_bitrate_kbps)),
                self.cc_shared.clone(),
                cc_generation,
            ));
        }

        // FEC sender task: bump the generation counter to retire any still-running
        // task from a previous setup call (ghost-writer guard, mirrors CC pattern).
        {
            let new_fec_gen = self.fec_generation.fetch_add(1, Ordering::AcqRel) + 1;
            self.fec_handle = Some(FecSenderHandle::spawn_for_channel(
                new_fec_gen,
                Arc::clone(&self.fec_generation),
                inner.video_fec_channel.clone(),
                Arc::clone(&self.needs_idr),
            ));
        }

        // QU relay task: same ghost-writer guard pattern — bump generation to retire
        // any prior relay task, bind a new listener, and spawn dormant.
        // Dormant cost: idle TcpListener + epoch tracking only (zero DataChannel
        // traffic until the client sends QU_SUBSCRIBE).
        {
            let new_qu_gen = self.qu_generation.fetch_add(1, Ordering::AcqRel) + 1;
            match QuRelayHandle::spawn(
                new_qu_gen,
                Arc::clone(&self.qu_generation),
                Arc::new(DataChannelSink(inner.video_qu_channel.clone())),
            )
            .await
            {
                Ok(handle) => {
                    info!("[QuRelay] relay bound at {}", handle.local_addr());
                    self.qu_handle = Some(handle);
                }
                Err(e) => {
                    error!("[QuRelay] failed to bind relay listener: {e}");
                }
            }
        }

        let needs_idr = self.needs_idr.clone();
        let target_bitrate_kbps = self.target_bitrate_kbps.clone();
        let configured_bitrate_kbps = self.configured_bitrate_kbps;
        let cc_shared = self.cc_shared.clone();
        if let Err(err) = self
            .sender
            .create_track(
                TrackLocalStaticRTP::new(
                    codec.capability.clone(),
                    "video".to_string(),
                    "moonlight".to_string(),
                )
                .into(),
                {
                    let needs_idr = needs_idr.clone();
                    let target_bitrate_kbps = target_bitrate_kbps.clone();
                    // ABR controller lives in this RTCP loop (the sole REMB reader).
                    // `None` disables adaptation when no ceiling was configured.
                    let mut abr = (configured_bitrate_kbps > 0).then(|| {
                        AbrController::new(AbrConfig::from_ceiling(configured_bitrate_kbps))
                    });

                    move |packet| {
                        let packet = packet.as_any();

                        if packet.is::<PictureLossIndication>() {
                            needs_idr.store(true, Ordering::Release);
                        }
                        if let Some(remb) = packet.downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                        {
                            // Feature #1: the base stack discarded this REMB estimate
                            // ("Moonlight doesn't support dynamic bitrate changing").
                            // We smooth it into a target bitrate here; applying that
                            // target to the Sunshine encoder is path A (pending a host
                            // patch) — see crate::abr and docs/ROADMAP.md M2.
                            if let Some(abr) = abr.as_mut() {
                                // REMB `bitrate` is bits/sec (f32); convert to kbps.
                                let observed_kbps =
                                    if remb.bitrate.is_finite() && remb.bitrate > 0.0 {
                                        (remb.bitrate / 1000.0) as u32
                                    } else {
                                        0
                                    };
                                let target = abr.observe(observed_kbps);
                                target_bitrate_kbps.store(target, Ordering::Release);
                                trace!("[ABR] REMB {observed_kbps} kbps -> target {target} kbps");
                            }
                        }
                        if let Some(rr) = packet.downcast_ref::<ReceiverReport>() {
                            // Feature #1: packet loss is a faster congestion signal
                            // than REMB's bandwidth estimate — cut the target
                            // immediately on a loss spike so the send queue doesn't
                            // build and mangle frames. Use the worst reception block
                            // this interval; `fraction_lost` is fixed-point /256
                            // (RFC 3550). Applying the target still needs path A.
                            if let Some(abr) = abr.as_mut()
                                && let Some(max_lost) =
                                    rr.reports.iter().map(|r| r.fraction_lost).max()
                            {
                                // Mirror the loss into the CC mailbox; the video
                                // sender loop drains it before its next frame's
                                // delay sample (same enable gate as ABR). Tagged
                                // with this setup's generation so a ghost RTCP
                                // reader from a replaced stream cannot post.
                                cc_shared.post_loss_for(cc_generation, max_lost);
                                let loss = max_lost as f64 / 256.0;
                                let target = abr.observe_loss(loss);
                                target_bitrate_kbps.store(target, Ordering::Release);
                                if max_lost > 0 {
                                    trace!(
                                        "[ABR] RR loss {:.1}% -> target {target} kbps",
                                        loss * 100.0
                                    );
                                }
                            }
                        }
                    }
                },
            )
            .await
        {
            let message = format!(
                "Failed to create video track with format {format:?} and codec \"{codec:?}\": {err:?}"
            );
            error!("{}", message);

            if let Err(err) = inner
                .event_sender
                .send(TransportEvent::SendIpc(StreamerIpcMessage::WebSocket(
                    StreamServerMessage::DebugLog {
                        message,
                        ty: Some(LogMessageType::FatalDescription),
                    },
                )))
                .await
            {
                warn!("Failed to send error to client: {err}");
            }
            return false;
        }

        self.clock_rate = codec.capability.clock_rate;

        self.codec = match format {
            // -- H264
            VideoFormat::H264 | VideoFormat::H264High8_444 => Some(VideoCodec::H264 {
                nal_reader: H264Reader::new(Cursor::new(Vec::new()), 0),
                payloader: Default::default(),
            }),
            // -- H265
            VideoFormat::H265
            | VideoFormat::H265Main10
            | VideoFormat::H265Rext8_444
            | VideoFormat::H265Rext10_444 => Some(VideoCodec::H265 {
                nal_reader: H265Reader::new(Cursor::new(Vec::new()), 0),
                payloader: Default::default(),
            }),
            // -- AV1
            VideoFormat::Av1Main8
            | VideoFormat::Av1Main10
            | VideoFormat::Av1High8_444
            | VideoFormat::Av1High10_444 => Some(VideoCodec::Av1 {
                payloader: Default::default(),
            }),
        };

        true
    }

    /// Forward an ACK message received on `video_fec_ack` to the FEC sender task.
    /// No-op before `setup()` (fec_handle is None).
    pub(super) fn handle_fec_ack(&self, msg: AckMsg) {
        if let Some(handle) = &self.fec_handle {
            handle.forward_ack(msg);
        }
    }

    /// Forward a raw `video_qu` DataChannel message from the client to the QU relay.
    /// No-op before `setup()` (qu_handle is None).
    pub(super) fn handle_qu_msg(&self, data: Bytes) {
        if let Some(handle) = &self.qu_handle {
            handle.forward_client_msg(data);
        }
    }

    pub async fn send_decode_unit(&mut self, unit: &VideoDecodeUnit<&[u8]>) -> DecodeResult {
        let timestamp = (unit.timestamp.as_nanos() * 90000 / 1_000_000_000) as u32;

        let mut full_frame = Vec::new();
        for buffer in &unit.buffers {
            full_frame.extend_from_slice(buffer.data);
        }

        let important = matches!(unit.frame_type, FrameType::Idr);
        // This is the encoded Moonlight decode-unit size before Annex-B parsing,
        // filler removal, or RTP packetization.
        if let Some(metrics) = self.sender.metrics() {
            metrics.record_encoded_frame(full_frame.len(), important);
        }

        // FEC tap: enqueue the pre-Annex-B full frame into the FEC sender.
        // is_active() is one AtomicBool::load — the only per-frame overhead
        // when the client has not yet subscribed (default dormant state).
        // timestamp_us is truncated to u32, matching the existing WS data path.
        let mut fec_carried = false;
        if let Some(fec) = &self.fec_handle
            && fec.is_active()
        {
            let ts_us = unit.timestamp.as_micros() as u32;
            fec_carried = fec
                .enqueue(Bytes::copy_from_slice(&full_frame), important, ts_us)
                .await;
        }

        // FEC-primary client: the frame is already on the wire via the
        // video_fec DataChannel — skip the duplicate RTP-track send
        // (otherwise every frame ships twice: RTP + FEC ≈ 2.2× nominal
        // wire; field finding 2026-07-16). The needs_idr consumption
        // below still runs, so loss-recovery IDR requests are unaffected.
        if skip_rtp_track(self.video_over_fec_only, fec_carried) {
            if self
                .needs_idr
                .compare_exchange_weak(true, false, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return DecodeResult::NeedIdr;
            }
            return DecodeResult::Ok;
        }

        match &mut self.codec {
            // -- H264
            Some(VideoCodec::H264 {
                nal_reader,
                payloader,
            }) => {
                nal_reader.reset(Cursor::new(full_frame));

                while let Ok(Some(nal)) = nal_reader.next_nal() {
                    trace!(
                        target: "video::header",
                        nal_start_code = ?nal.start_code,
                        nal_header = ?nal.header,
                        "h264 header"
                    );
                    trace!(
                        target: "video::nalu",
                        nal_data = ?&nal.full,
                        "h264 nalu"
                    );

                    if nal.header.nal_unit_type == h264::NalUnitType::FillerData {
                        trace!(target: "video","Ignoring nal because it's filler data: {:?}", nal.header);
                        continue;
                    }

                    let data = trim_bytes_to_range(
                        nal.full,
                        nal.header_range.start..nal.payload_range.end,
                    );

                    self.samples.push(data);
                }

                send_single_frame(
                    &mut self.samples,
                    &mut self.sender,
                    payloader,
                    timestamp,
                    important,
                    &self.needs_idr,
                )
                .await;
            }
            // -- H265
            Some(VideoCodec::H265 {
                nal_reader,
                payloader,
            }) => {
                nal_reader.reset(Cursor::new(full_frame));

                while let Ok(Some(nal)) = nal_reader.next_nal() {
                    trace!(
                        target: "video::header",
                        nal_start_code = ?nal.start_code,
                        nal_header = ?nal.header,
                        "h265 header"
                    );
                    trace!(
                        target: "video::nalu",
                        nal_data = ?&nal.full,
                        "h265 nalu"
                    );

                    let data = trim_bytes_to_range(
                        nal.full,
                        nal.header_range.start..nal.payload_range.end,
                    );

                    self.samples.push(data);
                }

                send_single_frame(
                    &mut self.samples,
                    &mut self.sender,
                    payloader,
                    timestamp,
                    important,
                    &self.needs_idr,
                )
                .await;
            }
            // -- AV1
            Some(VideoCodec::Av1 { payloader }) => {
                self.samples.push(BytesMut::from(full_frame.as_slice()));

                send_single_frame(
                    &mut self.samples,
                    &mut self.sender,
                    payloader,
                    timestamp,
                    important,
                    &self.needs_idr,
                )
                .await;
            }
            None => {
                warn!("Failed to send decode unit because of missing codec!");
            }
        }

        if self
            .needs_idr
            .compare_exchange_weak(true, false, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return DecodeResult::NeedIdr;
        }

        DecodeResult::Ok
    }
}

/// FEC-primary skip decision (pure, unit-tested): the duplicate RTP-track
/// send is skipped only when the client declared `video_over_fec_only`
/// (StartStream) AND the frame actually went out on the FEC channel this
/// call — never skip before the FEC subscribe lands, or the client would
/// be blind during session startup.
fn skip_rtp_track(fec_only: bool, fec_carried: bool) -> bool {
    fec_only && fec_carried
}

pub fn register_video_codecs(media_engine: &mut MediaEngine) -> Result<(), webrtc::Error> {
    for format in VideoFormat::all() {
        let Some(codec) = video_format_to_codec(format) else {
            continue;
        };
        debug!(
            "Registering Video Format {format:?}, Codec: {:?}",
            codec.capability
        );

        media_engine.register_codec(codec, RTPCodecType::Video)?;
    }

    Ok(())
}

async fn send_single_frame(
    samples: &mut Vec<BytesMut>,
    sender: &mut TrackLocalSender<SequencedTrackLocalStaticRTP>,
    payloader: &mut impl Payloader,
    timestamp: u32,
    important: bool,
    needs_idr: &AtomicBool,
) {
    if important {
        sender.clear_queue(false).await;
    }

    let mut peekable = samples.drain(..).peekable();

    let mut frame_samples = Vec::new();
    while let Some(sample) = peekable.next() {
        let packets = match packetize(
            payloader,
            RTP_OUTBOUND_MTU,
            0, // is set in the write fn
            timestamp,
            &sample.freeze(),
            peekable.peek().is_none(),
        ) {
            Ok(value) => value,
            Err(err) => {
                warn!("failed to packetize packet: {err}");
                continue;
            }
        };

        frame_samples.extend(packets);
    }

    if !sender.send_samples(frame_samples, important).await {
        sender.clear_queue(true).await;

        // We've dropped a frame (likely due to buffering)
        needs_idr.store(true, Ordering::Release);
    }
}

fn packetize(
    payloader: &mut impl Payloader,
    mtu: usize,
    sequence_number: u16,
    timestamp: u32,
    payload: &Bytes,
    end_has_marker: bool,
) -> Result<Vec<Packet>, anyhow::Error> {
    let payloads = payloader.payload(mtu - 12, payload)?;
    let payloads_len = payloads.len();
    let mut packets = Vec::with_capacity(payloads_len);
    for (i, payload) in payloads.into_iter().enumerate() {
        packets.push(Packet {
            header: Header {
                version: 2,
                padding: false,
                extension: false,
                marker: end_has_marker && i == payloads_len - 1,
                sequence_number,
                timestamp,
                payload_type: 0, // Value is handled when writing
                ssrc: 0,         // Value is handled when writing
                ..Default::default()
            },
            payload,
        });
    }

    Ok(packets)
}

fn video_format_to_codec(format: VideoFormat) -> Option<RTCRtpCodecParameters> {
    let rtcp_feedback = vec![
        RTCPFeedback {
            typ: "nack".to_string(),
            parameter: "".to_string(),
        },
        RTCPFeedback {
            typ: "nack".to_string(),
            parameter: "pli".to_string(),
        },
        RTCPFeedback {
            typ: "goog-remb".to_string(),
            parameter: "".to_string(),
        },
    ];

    match format {
        // -- H264 Constrained Baseline Profile
        VideoFormat::H264 => Some(RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_owned(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
            payload_type: 96,
            ..Default::default()
        }),
        // -- H264 High Profile
        VideoFormat::H264High8_444 => Some(RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032"
                        .to_owned(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
            payload_type: 97,
            ..Default::default()
        }),

        // -- H265 Main Profile
        VideoFormat::H265 => Some(RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_HEVC.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
            payload_type: 98,
            ..Default::default()
        }),
        // -- H265 Main10 Profile
        VideoFormat::H265Main10 => Some(RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_HEVC.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "profile-id=2;tier-flag=0;level-id=93;tx-mode=SRST".to_owned(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
            payload_type: 99,
            ..Default::default()
        }),
        // -- H265 RExt 4:4:4 8-bit
        VideoFormat::H265Rext8_444 => Some(RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_HEVC.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "profile-id=4;tier-flag=0;level-id=120;tx-mode=SRST".to_owned(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
            payload_type: 100,
            ..Default::default()
        }),
        // -- H265 RExt 4:4:4 10-bit
        VideoFormat::H265Rext10_444 => Some(RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_HEVC.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "profile-id=5;tier-flag=0;level-id=93;tx-mode=SRST".to_owned(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
            payload_type: 101,
            ..Default::default()
        }),

        // -- Av1
        VideoFormat::Av1Main8 | VideoFormat::Av1Main10 => Some(RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_AV1.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "profile=0".to_owned(),
                rtcp_feedback: rtcp_feedback.clone(),
            },
            payload_type: 102,
            ..Default::default()
        }),
        // Sunshine's supported hardware AV1 encoders currently emit Main
        // profile 4:2:0. Do not negotiate High profile 4:4:4 merely because a
        // browser decoder reports it; that creates a session the host cannot
        // truthfully satisfy.
        VideoFormat::Av1High8_444 | VideoFormat::Av1High10_444 => None,
    }
}

fn trim_bytes_to_range(mut buf: BytesMut, range: Range<usize>) -> BytesMut {
    if range.start > 0 {
        let _ = buf.split_to(range.start);
    }

    if range.end - range.start < buf.len() {
        let _ = buf.split_off(range.end - range.start);
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1_high_profile_is_not_advertised() {
        assert!(video_format_to_codec(VideoFormat::Av1High8_444).is_none());
        assert!(video_format_to_codec(VideoFormat::Av1High10_444).is_none());
    }

    #[test]
    fn fec_only_skips_rtp_track_only_once_fec_carries() {
        // Declared + carried -> skip the duplicate RTP send.
        assert!(skip_rtp_track(true, true));
        // Declared but FEC not yet active (pre-subscribe startup) -> keep
        // sending the track so the client is never blind.
        assert!(!skip_rtp_track(true, false));
        // Legacy clients (no declaration) never skip, FEC active or not.
        assert!(!skip_rtp_track(false, true));
        assert!(!skip_rtp_track(false, false));
    }

    #[test]
    fn av1_main_profile_remains_available() {
        for format in [VideoFormat::Av1Main8, VideoFormat::Av1Main10] {
            let codec = video_format_to_codec(format).expect("AV1 Main must remain supported");
            assert_eq!(codec.capability.mime_type, MIME_TYPE_AV1);
            assert_eq!(codec.capability.sdp_fmtp_line, "profile=0");
        }
    }
}
