use std::{
    collections::VecDeque,
    sync::{Arc, Weak},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use log::{debug, warn};
use tokio::{
    runtime::Handle,
    sync::{Mutex, Notify},
};
use webrtc::{
    api::media_engine::MediaEngine,
    media::Sample,
    peer_connection::RTCPeerConnection,
    rtcp::packet::Packet,
    rtp::{
        self,
        extension::{
            HeaderExtension, abs_send_time_extension::AbsSendTimeExtension,
            playout_delay_extension::PlayoutDelayExtension,
        },
    },
    rtp_transceiver::rtp_codec::{RTCRtpHeaderExtensionCapability, RTPCodecType},
    sdp::extmap::ABS_SEND_TIME_URI,
    track::track_local::{
        TrackLocal, track_local_static_rtp::TrackLocalStaticRTP,
        track_local_static_sample::TrackLocalStaticSample,
    },
};

use crate::cc::{CcController, CcShared, CcVerdict};
use crate::transport::metrics::{DurationAccumulator, VideoTransportMetrics};

const PLAYOUT_DELAY_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";

pub fn register_header_extensions(api_media: &mut MediaEngine) -> Result<(), webrtc::Error> {
    api_media.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: PLAYOUT_DELAY_URI.to_string(),
        },
        RTPCodecType::Video,
        None,
    )?;
    api_media.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: PLAYOUT_DELAY_URI.to_string(),
        },
        RTPCodecType::Audio,
        None,
    )?;

    api_media.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: ABS_SEND_TIME_URI.to_string(),
        },
        RTPCodecType::Video,
        None,
    )?;
    api_media.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: ABS_SEND_TIME_URI.to_string(),
        },
        RTPCodecType::Audio,
        None,
    )?;

    Ok(())
}

pub struct TrackLocalSender<Track>
where
    Track: TrackLike,
{
    runtime: Handle,
    peer: Weak<RTCPeerConnection>,
    channel_queue_size: usize,
    new_samples_notify: Arc<Notify>,
    queue: Arc<Mutex<VecDeque<FrameSamples<Track>>>>,
    metrics: Option<Arc<VideoTransportMetrics>>,
    /// Frame-delay congestion controller, installed by the video setup path
    /// only (audio senders never set it). Moved into the sample-sender task
    /// at `create_track`, which owns it for the track's lifetime.
    cc: Option<CcContext>,
}

/// The congestion controller plus its cross-task mailbox, owned by the
/// sample-sender loop. See `crate::cc` for the control law and the shared
/// state contract. `generation` tags every CcShared write so a superseded
/// sender task (stream re-setup spawns a new one without stopping the old)
/// cannot publish into the current stream's composition.
pub struct CcContext {
    controller: CcController,
    shared: Arc<CcShared>,
    generation: u32,
}

impl CcContext {
    pub fn new(controller: CcController, shared: Arc<CcShared>, generation: u32) -> Self {
        Self {
            controller,
            shared,
            generation,
        }
    }
}

struct FrameSamples<Track>
where
    Track: TrackLike,
{
    important: bool,
    enqueued_at: Instant,
    samples: Vec<Track::Sample>,
}

impl<Track> TrackLocalSender<Track>
where
    Track: TrackLike,
{
    pub fn new(runtime: Handle, peer: Weak<RTCPeerConnection>, channel_queue_size: usize) -> Self {
        Self {
            runtime,
            peer,
            channel_queue_size,
            new_samples_notify: Default::default(),
            queue: Default::default(),
            metrics: None,
            cc: None,
        }
    }

    pub fn new_with_metrics(
        runtime: Handle,
        peer: Weak<RTCPeerConnection>,
        channel_queue_size: usize,
        metrics: Arc<VideoTransportMetrics>,
    ) -> Self {
        Self {
            runtime,
            peer,
            channel_queue_size,
            new_samples_notify: Default::default(),
            queue: Default::default(),
            metrics: Some(metrics),
            cc: None,
        }
    }

    pub fn metrics(&self) -> Option<&VideoTransportMetrics> {
        self.metrics.as_deref()
    }

    /// Install the frame-delay congestion controller for the next
    /// `create_track` call. Video-only; called at stream setup when a bitrate
    /// ceiling is configured (the same gate that enables ABR).
    pub fn set_congestion_controller(&mut self, cc: CcContext) {
        self.cc = Some(cc);
    }

    pub async fn create_track(
        &mut self,
        track: Track,
        mut on_packet: impl FnMut(Box<dyn Packet + Send + Sync>) + Send + 'static,
    ) -> Result<(), anyhow::Error> {
        let Some(peer) = self.peer.upgrade() else {
            return Err(anyhow!(
                "Failed to create track because of missing webrtc peer!"
            ));
        };

        let track = Arc::new(track);

        let new_samples_notify = self.new_samples_notify.clone();
        let queue = Arc::downgrade(&self.queue);
        let metrics = self.metrics.clone();
        let cc = self.cc.take();
        self.runtime.spawn({
            let track = track.clone();
            async move {
                sample_sender(track, &new_samples_notify, queue, metrics, cc).await;
            }
        });

        let track_sender = peer.add_track(track.track()).await?;

        // Read incoming RTCP packets
        // Before these packets are returned they are processed by interceptors. For things
        // like NACK this needs to be called.
        self.runtime.spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            while let Ok((packets, _)) = track_sender.read(&mut rtcp_buf).await {
                for packet in packets {
                    on_packet(packet);
                }
            }
        });

        Ok(())
    }

    /// Returns if the frame will be delivered
    pub async fn send_samples(&self, samples: Vec<Track::Sample>, important: bool) -> bool {
        let rtp_payload_bytes = samples.iter().map(Track::sample_payload_len).sum();
        let mut queue = self.queue.lock().await;

        let outcome = enqueue_frame(
            &mut queue,
            self.channel_queue_size,
            FrameSamples {
                important,
                enqueued_at: Instant::now(),
                samples,
            },
        );
        if let Some(metrics) = self.metrics.as_ref() {
            metrics.record_enqueue(
                outcome.accepted,
                important,
                rtp_payload_bytes,
                outcome.replaced_frames,
                queue.len(),
            );
        }

        if !outcome.accepted {
            return false;
        }

        // There is one sender task per queue. `notify_one()` retains a permit
        // when the sender is between the empty-queue check and `notified()`, so
        // the final frame of a burst cannot be stranded until another enqueue.
        self.new_samples_notify.notify_one();

        true
    }

    /// Returns if the frame will be delivered
    pub async fn clear_queue(&self, clear_important: bool) {
        let mut queue = self.queue.lock().await;
        let previous_len = queue.len();

        if clear_important {
            queue.clear();
        } else {
            queue.retain(|frame| frame.important);
        }

        if let Some(metrics) = self.metrics.as_ref() {
            metrics.record_clear(previous_len - queue.len(), queue.len());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EnqueueOutcome {
    accepted: bool,
    replaced_frames: usize,
}

/// Enqueue a complete frame while keeping latency and memory bounded.
///
/// A new important frame (currently an IDR) supersedes every queued frame: older
/// dependent frames cannot be decoded after skipping to it, and older IDRs are
/// no longer useful for recovery. It is therefore always accepted as the sole
/// queued frame, even when the configured queue size is zero. Non-important
/// frames respect the configured limit exactly.
fn enqueue_frame<Track>(
    queue: &mut VecDeque<FrameSamples<Track>>,
    queue_size: usize,
    frame: FrameSamples<Track>,
) -> EnqueueOutcome
where
    Track: TrackLike,
{
    if frame.important {
        let replaced_frames = queue.len();
        queue.clear();
        queue.push_front(frame);
        return EnqueueOutcome {
            accepted: true,
            replaced_frames,
        };
    }

    if queue.len() >= queue_size {
        return EnqueueOutcome {
            accepted: false,
            replaced_frames: 0,
        };
    }

    queue.push_front(frame);
    EnqueueOutcome {
        accepted: true,
        replaced_frames: 0,
    }
}

async fn sample_sender<Track>(
    track: Arc<Track>,
    new_samples_notify: &Notify,
    queue: Weak<Mutex<VecDeque<FrameSamples<Track>>>>,
    metrics: Option<Arc<VideoTransportMetrics>>,
    mut cc: Option<CcContext>,
) where
    Track: TrackLike,
{
    // Monotonic epoch for CC timestamps. Created before any frame is
    // processed, so `saturating_duration_since(epoch)` never truncates.
    let cc_epoch = Instant::now();
    loop {
        let frame = {
            let Some(queue) = queue.upgrade() else {
                debug!("no sample queue available: stopping to submit samples");
                break;
            };

            let mut queue = queue.lock().await;
            let Some(new_frame) = queue.pop_back() else {
                drop(queue); // Important: drop the mutex

                new_samples_notify.notified().await;
                continue;
            };

            if let Some(metrics) = metrics.as_ref() {
                let rtp_packets = new_frame.samples.len();
                let rtp_payload_bytes = new_frame
                    .samples
                    .iter()
                    .map(Track::sample_payload_len)
                    .sum();
                metrics.record_dequeue(
                    rtp_packets,
                    rtp_payload_bytes,
                    queue.len(),
                    new_frame.enqueued_at.elapsed(),
                );
            }

            new_frame
        };
        let mut in_flight_frame = InFlightFrame::new(metrics.as_deref());

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let now_secs = now.as_secs() as f64 + now.subsec_nanos() as f64 * 1e-9;
        let abs_send_time: u64 = (now_secs * 262_144.0) as u64;

        let send_started = Instant::now();
        // Frame payload bytes whose track write was actually attempted
        // (written or failed). Stays 0 for fully-skipped frames (track not
        // ready / paused): their near-zero "service time" is not congestion
        // evidence and must not feed the controller as deflate signal.
        let mut cc_attempted_bytes: u64 = 0;

        for sample in frame.samples {
            let rtp_payload_bytes = Track::sample_payload_len(&sample);
            let write_started = Instant::now();
            let result = track
                .write_with_extensions(
                    sample,
                    &[
                        HeaderExtension::PlayoutDelay(PlayoutDelayExtension::new(0, 0)),
                        HeaderExtension::AbsSendTime(AbsSendTimeExtension {
                            timestamp: abs_send_time,
                        }),
                    ],
                )
                .await;
            let write_latency = write_started.elapsed();
            match result {
                Ok(TrackWriteOutcome::Written) => {
                    if let Some(metrics) = metrics.as_ref() {
                        metrics.record_write_succeeded(rtp_payload_bytes);
                    }
                    in_flight_frame.record_write_latency(write_latency);
                    cc_attempted_bytes += rtp_payload_bytes as u64;
                }
                Ok(TrackWriteOutcome::Skipped) => {
                    if let Some(metrics) = metrics.as_ref() {
                        metrics.record_write_skipped(rtp_payload_bytes);
                    }
                }
                Err(err) => {
                    if let Some(metrics) = metrics.as_ref() {
                        metrics.record_write_failed(rtp_payload_bytes);
                    }
                    in_flight_frame.record_write_latency(write_latency);
                    warn!("[Stream]: track.write_sample failed: {err}");
                    cc_attempted_bytes += rtp_payload_bytes as u64;
                }
            }
        }

        if let Some(cc) = cc.as_mut()
            && cc_attempted_bytes > 0
        {
            // RR loss reacts immediately (bypasses inflate accumulation);
            // apply the pending report before this frame's delay sample. The
            // resulting target is published once below: `on_frame` always
            // returns the current target (loss decrease included), even on
            // a Skipped verdict.
            if let Some(loss) = cc.shared.take_loss_for(cc.generation) {
                cc.controller.on_loss_report(loss);
            }

            let send_start_us = send_started.saturating_duration_since(cc_epoch).as_micros() as u64;
            let send_done_us = Instant::now()
                .saturating_duration_since(cc_epoch)
                .as_micros() as u64;
            let (target, verdict) = cc.controller.on_frame(
                send_start_us,
                send_done_us,
                cc_attempted_bytes,
                cc.shared.frame_interval_us(),
            );
            cc.shared.publish_target_for(cc.generation, target);
            if matches!(verdict, CcVerdict::Decrease | CcVerdict::Increase) {
                debug!("[CC] {verdict:?} -> target {target} kbps");
            }
        }
    }
}

/// Keeps the in-flight gauge truthful if the sender future is cancelled while
/// awaiting a track write.
struct InFlightFrame<'a> {
    metrics: Option<&'a VideoTransportMetrics>,
    write_latency: DurationAccumulator,
}

impl<'a> InFlightFrame<'a> {
    fn new(metrics: Option<&'a VideoTransportMetrics>) -> Self {
        Self {
            metrics,
            write_latency: DurationAccumulator::default(),
        }
    }

    fn record_write_latency(&mut self, latency: std::time::Duration) {
        if self.metrics.is_some() {
            self.write_latency.record(latency);
        }
    }
}

impl Drop for InFlightFrame<'_> {
    fn drop(&mut self) {
        if let Some(metrics) = self.metrics {
            metrics.record_frame_write_finished(self.write_latency);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackWriteOutcome {
    Written,
    Skipped,
}

pub trait TrackLike: Send + Sync + 'static {
    type Sample: Send + 'static;

    fn sample_payload_len(sample: &Self::Sample) -> usize;

    fn write_with_extensions(
        &self,
        sample: Self::Sample,
        extensions: &[HeaderExtension],
    ) -> impl Future<Output = Result<TrackWriteOutcome, anyhow::Error>> + Send;

    fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static>;
}

impl TrackLike for TrackLocalStaticSample {
    type Sample = Sample;

    fn sample_payload_len(sample: &Self::Sample) -> usize {
        sample.data.len()
    }

    async fn write_with_extensions(
        &self,
        sample: Self::Sample,
        extensions: &[HeaderExtension],
    ) -> Result<TrackWriteOutcome, anyhow::Error> {
        self.write_sample_with_extensions(&sample, extensions)
            .await
            .map_err(anyhow::Error::from)
            .map(|_| TrackWriteOutcome::Written)
    }

    fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static> {
        self
    }
}

pub struct SequencedTrackLocalStaticRTP {
    track: Arc<TrackLocalStaticRTP>,
    sequence_number: Mutex<u16>,
}

impl From<TrackLocalStaticRTP> for SequencedTrackLocalStaticRTP {
    fn from(value: TrackLocalStaticRTP) -> Self {
        Self {
            track: Arc::new(value),
            sequence_number: Mutex::new(0),
        }
    }
}

impl TrackLike for SequencedTrackLocalStaticRTP {
    type Sample = rtp::packet::Packet;

    fn sample_payload_len(sample: &Self::Sample) -> usize {
        sample.payload.len()
    }

    async fn write_with_extensions(
        &self,
        mut sample: Self::Sample,
        extensions: &[HeaderExtension],
    ) -> Result<TrackWriteOutcome, anyhow::Error> {
        let (any_paused, all_paused) = (
            self.track.any_binding_paused().await,
            self.track.all_binding_paused().await,
        );

        if all_paused {
            // Abort already here to not increment sequence numbers.
            return Ok(TrackWriteOutcome::Skipped);
        }
        if any_paused {
            warn!("WebRTC: not all paused but any paused");
        }

        let mut sequence_number = self.sequence_number.lock().await;
        sample.header.sequence_number = *sequence_number;
        *sequence_number = sequence_number.wrapping_add(1);

        self.track
            .write_rtp_with_extensions(&sample, extensions)
            .await
            .map_err(anyhow::Error::from)
            .map(|_| TrackWriteOutcome::Written)
    }

    fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static> {
        self.track.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeTrack {
        calls: AtomicUsize,
        completed: Notify,
    }

    impl TrackLike for FakeTrack {
        type Sample = usize;

        fn sample_payload_len(sample: &Self::Sample) -> usize {
            *sample
        }

        async fn write_with_extensions(
            &self,
            _sample: Self::Sample,
            _extensions: &[HeaderExtension],
        ) -> Result<TrackWriteOutcome, anyhow::Error> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            let result = match call {
                0 => Ok(TrackWriteOutcome::Written),
                1 => Err(anyhow!("synthetic track write failure")),
                _ => Ok(TrackWriteOutcome::Skipped),
            };
            if call == 2 {
                self.completed.notify_one();
            }
            result
        }

        fn track(self: Arc<Self>) -> Arc<dyn TrackLocal + Send + Sync + 'static> {
            panic!("the fake track is never attached to a peer")
        }
    }

    fn frame(important: bool) -> FrameSamples<TrackLocalStaticSample> {
        FrameSamples {
            important,
            enqueued_at: Instant::now(),
            samples: Vec::new(),
        }
    }

    #[test]
    fn non_important_frames_respect_the_exact_queue_limit() {
        let mut queue = VecDeque::new();

        assert!(enqueue_frame(&mut queue, 2, frame(false)).accepted);
        assert!(enqueue_frame(&mut queue, 2, frame(false)).accepted);
        assert!(!enqueue_frame(&mut queue, 2, frame(false)).accepted);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn newest_important_frame_supersedes_all_queued_frames() {
        let mut queue = VecDeque::new();
        assert!(enqueue_frame(&mut queue, 3, frame(true)).accepted);
        assert!(enqueue_frame(&mut queue, 3, frame(false)).accepted);
        assert!(enqueue_frame(&mut queue, 3, frame(false)).accepted);

        let outcome = enqueue_frame(&mut queue, 3, frame(true));

        assert!(outcome.accepted);
        assert_eq!(outcome.replaced_frames, 3);
        assert_eq!(queue.len(), 1);
        assert!(queue.front().is_some_and(|queued| queued.important));
    }

    #[test]
    fn zero_sized_queue_still_accepts_one_recovery_frame() {
        let mut queue = VecDeque::new();

        assert!(!enqueue_frame(&mut queue, 0, frame(false)).accepted);
        assert!(enqueue_frame(&mut queue, 0, frame(true)).accepted);
        assert_eq!(queue.len(), 1);
        assert!(!enqueue_frame(&mut queue, 0, frame(false)).accepted);
        assert_eq!(queue.len(), 1);
    }

    #[tokio::test]
    async fn sender_counts_track_write_results_instead_of_dequeue_as_delivery() {
        let metrics = Arc::new(VideoTransportMetrics::new(2));
        let frame = FrameSamples {
            important: false,
            enqueued_at: Instant::now()
                .checked_sub(std::time::Duration::from_millis(2))
                .expect("test instant must have two milliseconds of history"),
            samples: vec![100, 200, 300],
        };
        let queue = Arc::new(Mutex::new(VecDeque::from([frame])));
        metrics.record_enqueue(true, false, 600, 0, 1);

        let track = Arc::new(FakeTrack {
            calls: AtomicUsize::new(0),
            completed: Notify::new(),
        });
        let sender_notify = Arc::new(Notify::new());
        let task = tokio::spawn({
            let track = track.clone();
            let sender_notify = sender_notify.clone();
            let queue = Arc::downgrade(&queue);
            let metrics = metrics.clone();
            async move {
                sample_sender(track, &sender_notify, queue, Some(metrics), None).await;
            }
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            track.completed.notified(),
        )
        .await
        .expect("sender did not process the frame");
        drop(queue);
        sender_notify.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("sender did not stop after its queue was dropped")
            .expect("sender task panicked");

        let stats = metrics.take_snapshot();
        assert_eq!(stats.frames_dequeued, 1);
        assert_eq!(stats.rtp_packets_dequeued, 3);
        assert_eq!(stats.rtp_payload_bytes_dequeued, 600);
        assert_eq!(stats.rtp_packets_write_succeeded, 1);
        assert_eq!(stats.rtp_payload_bytes_write_succeeded, 100);
        assert_eq!(stats.rtp_packets_write_failed, 1);
        assert_eq!(stats.rtp_payload_bytes_write_failed, 200);
        assert_eq!(stats.rtp_packets_write_skipped, 1);
        assert_eq!(stats.rtp_payload_bytes_write_skipped, 300);
        assert_eq!(stats.rtp_write_latency_samples, 2);
        assert_eq!(stats.queue_wait_samples, 1);
        assert!(stats.queue_wait_min_ms >= 1.0);
        assert_eq!(stats.in_flight_frames, 0);
        assert_eq!(stats.in_flight_max_frames, 1);
    }
}
