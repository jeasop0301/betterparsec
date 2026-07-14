use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use common::api_bindings::{StatsHostProcessingLatency, StreamerStatsUpdate};
use log::{debug, error, warn};
use moonlight_common::stream::{
    c::bindings::EstimatedRttInfo,
    video::{
        DecodeResult, VideoCapabilities, VideoDecodeUnit, VideoDecoder, VideoFormats, VideoSetup,
    },
};

use crate::{
    StreamConnection,
    transport::{OutboundPacket, metrics::VideoTransportStats},
};

pub(crate) struct StreamVideoDecoder {
    pub(crate) stream: Weak<StreamConnection>,
    pub(crate) supported_formats: VideoFormats,
    pub(crate) stats: VideoStats,
}

impl VideoDecoder for StreamVideoDecoder {
    fn setup(&mut self, setup: VideoSetup) -> i32 {
        let Some(stream) = self.stream.upgrade() else {
            warn!("Failed to setup video because stream is deallocated");
            return -1;
        };

        {
            let mut stream_info = stream.stream_setup.blocking_lock();
            stream_info.video = Some(setup);
        }

        {
            stream.runtime.clone().block_on(async move {
                let mut sender = stream.transport_sender.lock().await;

                if let Some(sender) = sender.as_mut() {
                    sender.setup_video(setup).await
                } else {
                    error!("Failed to setup video because of missing transport!");
                    -1
                }
            })
        }
    }

    fn start(&mut self) {}
    fn stop(&mut self) {}

    fn submit_decode_unit(&mut self, unit: VideoDecodeUnit<&[u8]>) -> DecodeResult {
        let Some(stream) = self.stream.upgrade() else {
            warn!("Failed to send video decode unit because stream is deallocated");
            return DecodeResult::Ok;
        };

        let mut sender_guard = stream.transport_sender.blocking_lock();

        let start = Instant::now();

        let result = stream.runtime.block_on(async {
            if let Some(sender) = sender_guard.as_mut() {
                match sender.send_video_unit(unit.as_ref()).await {
                    Err(err) => {
                        warn!("Failed to send video decode unit: {err}");
                        DecodeResult::Ok
                    }
                    Ok(value) => value,
                }
            } else {
                debug!("Dropping video packet because of missing transport");

                DecodeResult::Ok
            }
        });

        let frame_processing_time = Instant::now() - start;
        let now = Instant::now();
        let report_due = self.stats.report_due(now);
        let transport_stats = report_due
            .then(|| {
                sender_guard
                    .as_ref()
                    .and_then(|sender| sender.take_video_transport_stats())
            })
            .flatten();
        drop(sender_guard);
        self.stats.analyze(
            &stream,
            &unit,
            frame_processing_time,
            now,
            report_due,
            transport_stats,
        );

        result
    }

    fn supported_formats(&self) -> VideoFormats {
        self.supported_formats
    }

    fn capabilities(&self) -> VideoCapabilities {
        // Default: slices_per_frame = None → maps to 0 in bits 24-31 of
        // CAPABILITY_SLICES_PER_FRAME, which Sunshine treats as 1 slice.
        // Activation and tuning are gated in the ROADMAP; when that config
        // surface is added, set slices_per_frame here instead of None.
        VideoCapabilities::default()
    }
}

#[derive(Debug)]
pub(crate) struct VideoStats {
    last_send: Option<Instant>,
    min_host_processing_latency: Duration,
    max_host_processing_latency: Duration,
    total_host_processing_latency: Duration,
    host_processing_frame_count: usize,
    min_streamer_processing_time: Duration,
    max_streamer_processing_time: Duration,
    total_streamer_processing_time: Duration,
    streamer_processing_time_frame_count: usize,
}

impl Default for VideoStats {
    fn default() -> Self {
        Self {
            // A full first interval avoids publishing a near-zero-rate sample
            // immediately after the decoder is constructed.
            last_send: Some(Instant::now()),
            min_host_processing_latency: Duration::MAX,
            max_host_processing_latency: Duration::ZERO,
            total_host_processing_latency: Duration::ZERO,
            host_processing_frame_count: 0,
            min_streamer_processing_time: Duration::MAX,
            max_streamer_processing_time: Duration::ZERO,
            total_streamer_processing_time: Duration::ZERO,
            streamer_processing_time_frame_count: 0,
        }
    }
}

impl VideoStats {
    fn report_due(&self, now: Instant) -> bool {
        self.last_send
            .is_none_or(|last_send| last_send + Duration::from_secs(1) < now)
    }

    fn analyze(
        &mut self,
        stream: &Arc<StreamConnection>,
        unit: &VideoDecodeUnit<&[u8]>,
        frame_processing_time: Duration,
        now: Instant,
        report_due: bool,
        transport_stats: Option<VideoTransportStats>,
    ) {
        if let Some(host_processing_latency) = unit.frame_processing_latency {
            self.min_host_processing_latency = self
                .min_host_processing_latency
                .min(host_processing_latency);
            self.max_host_processing_latency = self
                .max_host_processing_latency
                .max(host_processing_latency);
            self.total_host_processing_latency += host_processing_latency;
            self.host_processing_frame_count += 1;
        }

        self.min_streamer_processing_time =
            self.min_streamer_processing_time.min(frame_processing_time);
        self.max_streamer_processing_time =
            self.max_streamer_processing_time.max(frame_processing_time);
        self.total_streamer_processing_time += frame_processing_time;
        self.streamer_processing_time_frame_count += 1;

        // Send in 1 sec intervall
        if report_due {
            // Collect data
            let has_host_processing_latency = self.host_processing_frame_count > 0;
            let min_host_processing_latency = self.min_host_processing_latency;
            let max_host_processing_latency = self.max_host_processing_latency;
            let avg_host_processing_latency = self
                .total_host_processing_latency
                .checked_div(self.host_processing_frame_count as u32)
                .unwrap_or(Duration::ZERO);

            let min_streamer_processing_time = self.min_streamer_processing_time;
            let max_streamer_processing_time = self.max_streamer_processing_time;
            let avg_streamer_processing_time = self
                .total_streamer_processing_time
                .checked_div(self.streamer_processing_time_frame_count as u32)
                .unwrap_or(Duration::ZERO);

            // Send data
            let runtime = stream.runtime.clone();

            let stream = stream.clone();
            runtime.spawn(async move {
                stream
                    .try_send_packet(
                        OutboundPacket::Stats(StreamerStatsUpdate::Video {
                            host_processing_latency: has_host_processing_latency.then_some(
                                StatsHostProcessingLatency {
                                    min_host_processing_latency_ms: min_host_processing_latency
                                        .as_secs_f64()
                                        * 1000.0,
                                    max_host_processing_latency_ms: max_host_processing_latency
                                        .as_secs_f64()
                                        * 1000.0,
                                    avg_host_processing_latency_ms: avg_host_processing_latency
                                        .as_secs_f64()
                                        * 1000.0,
                                },
                            ),
                            min_streamer_processing_time_ms: min_streamer_processing_time
                                .as_secs_f64()
                                * 1000.0,
                            max_streamer_processing_time_ms: max_streamer_processing_time
                                .as_secs_f64()
                                * 1000.0,
                            avg_streamer_processing_time_ms: avg_streamer_processing_time
                                .as_secs_f64()
                                * 1000.0,
                        }),
                        "host / streamer processing latency",
                        false,
                    )
                    .await;

                if let Some(stats) = transport_stats {
                    stream
                        .try_send_packet(
                            OutboundPacket::Stats(StreamerStatsUpdate::VideoTransport {
                                interval_ms: stats.interval_ms,
                                queue_capacity_frames: saturating_u32(stats.queue_capacity_frames),
                                queue_depth_frames: saturating_u32(stats.queue_depth_frames),
                                queue_max_depth_frames: saturating_u32(
                                    stats.queue_max_depth_frames,
                                ),
                                in_flight_frames: saturating_u32(stats.in_flight_frames),
                                in_flight_max_frames: saturating_u32(stats.in_flight_max_frames),
                                encoded_frames_received: saturating_u32(
                                    stats.encoded_frames_received,
                                ),
                                encoded_payload_bytes_received: saturating_u32(
                                    stats.encoded_payload_bytes_received,
                                ),
                                frames_accepted: saturating_u32(stats.frames_accepted),
                                frames_rejected: saturating_u32(stats.frames_rejected),
                                frames_replaced: saturating_u32(stats.frames_replaced),
                                frames_cleared: saturating_u32(stats.frames_cleared),
                                frames_dropped: saturating_u32(stats.frames_dropped),
                                frames_dequeued: saturating_u32(stats.frames_dequeued),
                                idr_frames_received: saturating_u32(stats.idr_frames_received),
                                idr_encoded_payload_bytes_received: saturating_u32(
                                    stats.idr_encoded_payload_bytes_received,
                                ),
                                idr_frames_accepted: saturating_u32(stats.idr_frames_accepted),
                                idr_rtp_payload_bytes_accepted: saturating_u32(
                                    stats.idr_rtp_payload_bytes_accepted,
                                ),
                                rtp_packets_dequeued: saturating_u32(stats.rtp_packets_dequeued),
                                rtp_payload_bytes_dequeued: saturating_u32(
                                    stats.rtp_payload_bytes_dequeued,
                                ),
                                rtp_packets_write_succeeded: saturating_u32(
                                    stats.rtp_packets_write_succeeded,
                                ),
                                rtp_payload_bytes_write_succeeded: saturating_u32(
                                    stats.rtp_payload_bytes_write_succeeded,
                                ),
                                rtp_packets_write_failed: saturating_u32(
                                    stats.rtp_packets_write_failed,
                                ),
                                rtp_payload_bytes_write_failed: saturating_u32(
                                    stats.rtp_payload_bytes_write_failed,
                                ),
                                rtp_packets_write_skipped: saturating_u32(
                                    stats.rtp_packets_write_skipped,
                                ),
                                rtp_payload_bytes_write_skipped: saturating_u32(
                                    stats.rtp_payload_bytes_write_skipped,
                                ),
                                queue_wait_samples: saturating_u32(stats.queue_wait_samples),
                                queue_wait_min_ms: stats.queue_wait_min_ms,
                                queue_wait_max_ms: stats.queue_wait_max_ms,
                                queue_wait_avg_ms: stats.queue_wait_avg_ms,
                                rtp_write_latency_samples: saturating_u32(
                                    stats.rtp_write_latency_samples,
                                ),
                                rtp_write_latency_min_ms: stats.rtp_write_latency_min_ms,
                                rtp_write_latency_max_ms: stats.rtp_write_latency_max_ms,
                                rtp_write_latency_avg_ms: stats.rtp_write_latency_avg_ms,
                            }),
                            "video transport metrics",
                            false,
                        )
                        .await;
                }

                // Send RTT info
                let ml_stream_lock = stream.stream.read().await;
                if let Some(ml_stream) = ml_stream_lock.as_ref() {
                    let rtt = ml_stream.estimated_rtt_info();
                    drop(ml_stream_lock);

                    match rtt {
                        Ok(EstimatedRttInfo { rtt, rtt_variance }) => {
                            stream
                                .try_send_packet(
                                    OutboundPacket::Stats(StreamerStatsUpdate::Rtt {
                                        rtt_ms: rtt.as_secs_f64() * 1000.0,
                                        rtt_variance_ms: rtt_variance.as_secs_f64() * 1000.0,
                                    }),
                                    "estimated rtt info",
                                    false,
                                )
                                .await;
                        }
                        Err(err) => {
                            warn!("failed to get estimated rtt info: {err:?}");
                        }
                    };
                }
            });

            // Clear data
            self.min_host_processing_latency = Duration::MAX;
            self.max_host_processing_latency = Duration::ZERO;
            self.total_host_processing_latency = Duration::ZERO;
            self.host_processing_frame_count = 0;
            self.min_streamer_processing_time = Duration::MAX;
            self.max_streamer_processing_time = Duration::ZERO;
            self.total_streamer_processing_time = Duration::ZERO;
            self.streamer_processing_time_frame_count = 0;

            self.last_send = Some(now);
        }
    }
}

fn saturating_u32(value: u64) -> u32 {
    value.try_into().unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_stats_minima_start_at_max_duration() {
        let stats = VideoStats::default();

        assert_eq!(stats.min_host_processing_latency, Duration::MAX);
        assert_eq!(stats.min_streamer_processing_time, Duration::MAX);
    }

    #[test]
    fn public_metric_counts_saturate_instead_of_wrapping() {
        assert_eq!(saturating_u32(42), 42);
        assert_eq!(saturating_u32(u64::from(u32::MAX) + 1), u32::MAX);
    }
}

/// Tests for CAPABILITY_SLICES_PER_FRAME packing (Limelight.h line 282).
/// The function under test lives in moonlight_common::stream::c::video and is
/// the single source of truth for translating VideoCapabilities.slices_per_frame
/// into C-layer capability bits 24-31.
#[cfg(test)]
mod slice_caps_tests {
    use moonlight_common::stream::{c::video::slices_per_frame_capability_bits, video::VideoCapabilities};

    // Domain: None/0/1 → top byte 0 (same as today, no slices advertised)
    #[test]
    fn none_yields_top_byte_zero() {
        assert_eq!(slices_per_frame_capability_bits(None), 0);
    }

    #[test]
    fn zero_treated_as_one_yields_top_byte_zero() {
        assert_eq!(slices_per_frame_capability_bits(Some(0)), 0);
    }

    #[test]
    fn one_yields_top_byte_zero() {
        assert_eq!(slices_per_frame_capability_bits(Some(1)), 0);
    }

    // Domain: 2..=255 → packed into bits 24-31
    #[test]
    fn two_packed_into_top_byte() {
        assert_eq!(slices_per_frame_capability_bits(Some(2)), 0x02000000);
    }

    #[test]
    fn four_packed_into_top_byte() {
        // CAPABILITY_SLICES_PER_FRAME(4) = ((4 as u8) << 24) = 0x04000000
        assert_eq!(slices_per_frame_capability_bits(Some(4)), 0x04000000);
        assert_eq!(
            slices_per_frame_capability_bits(Some(4)) & 0xFF000000,
            0x04000000
        );
    }

    #[test]
    fn max_u8_255_packed_into_top_byte() {
        assert_eq!(slices_per_frame_capability_bits(Some(255)), 0xFF000000);
    }

    // Domain: > 255 → clamped to 255
    #[test]
    fn oversized_256_clamped_to_255() {
        assert_eq!(slices_per_frame_capability_bits(Some(256)), 0xFF000000);
    }

    #[test]
    fn oversized_u32_max_clamped_to_255() {
        assert_eq!(slices_per_frame_capability_bits(Some(u32::MAX)), 0xFF000000);
    }

    #[test]
    fn slice_bits_disjoint_from_low_capability_bits() {
        // All known capability flags live in bits 0-6 (0x7F).
        // Slice bits are in bits 24-31. Verify no overlap.
        let all_low_bits: u32 = 0x7F;
        for slices in [2u32, 4, 255] {
            let slice_bits = slices_per_frame_capability_bits(Some(slices));
            assert_eq!(
                slice_bits & all_low_bits,
                0,
                "slice_bits for {slices} must not overlap low capability bits"
            );
        }
    }

    /// Regression pin: the shipped default path (StreamVideoDecoder returns
    /// VideoCapabilities::default() with slices_per_frame = None) must produce
    /// 0 in bits 24-31 — bit-identical to behavior before this change.
    #[test]
    fn default_videocapabilities_produces_zero_slice_bits() {
        let caps = VideoCapabilities::default();
        assert!(caps.slices_per_frame.is_none(), "default must have no slice preference");
        assert_eq!(
            slices_per_frame_capability_bits(caps.slices_per_frame),
            0,
            "default capabilities must yield 0 in CAPABILITY_SLICES_PER_FRAME bits"
        );
    }
}
