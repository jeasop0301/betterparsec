use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Interval metrics for the application-owned WebRTC video send queue.
///
/// Counters are reset by [`VideoTransportMetrics::take_snapshot`]. Queue depth
/// is an instantaneous gauge. RTP payload counters deliberately exclude RTP,
/// header-extension, SRTP, UDP/IP, ICE, and retransmission overhead.
#[derive(Debug)]
pub(crate) struct VideoTransportMetrics {
    queue_capacity_frames: u64,
    queue_depth_frames: AtomicU64,
    queue_max_depth_frames: AtomicU64,
    in_flight_frames: AtomicU64,
    in_flight_max_frames: AtomicU64,
    encoded_frames_received: AtomicU64,
    encoded_payload_bytes_received: AtomicU64,
    frames_accepted: AtomicU64,
    frames_rejected: AtomicU64,
    frames_replaced: AtomicU64,
    frames_cleared: AtomicU64,
    frames_dequeued: AtomicU64,
    idr_frames_received: AtomicU64,
    idr_encoded_payload_bytes_received: AtomicU64,
    idr_frames_accepted: AtomicU64,
    idr_rtp_payload_bytes_accepted: AtomicU64,
    rtp_packets_dequeued: AtomicU64,
    rtp_payload_bytes_dequeued: AtomicU64,
    rtp_packets_write_succeeded: AtomicU64,
    rtp_payload_bytes_write_succeeded: AtomicU64,
    rtp_packets_write_failed: AtomicU64,
    rtp_payload_bytes_write_failed: AtomicU64,
    rtp_packets_write_skipped: AtomicU64,
    rtp_payload_bytes_write_skipped: AtomicU64,
    queue_wait: Mutex<DurationAccumulator>,
    rtp_write_latency: Mutex<DurationAccumulator>,
    last_snapshot: Mutex<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VideoTransportStats {
    pub interval_ms: f64,
    pub queue_capacity_frames: u64,
    pub queue_depth_frames: u64,
    pub queue_max_depth_frames: u64,
    pub in_flight_frames: u64,
    pub in_flight_max_frames: u64,
    pub encoded_frames_received: u64,
    pub encoded_payload_bytes_received: u64,
    pub frames_accepted: u64,
    pub frames_rejected: u64,
    pub frames_replaced: u64,
    pub frames_cleared: u64,
    pub frames_dropped: u64,
    pub frames_dequeued: u64,
    pub idr_frames_received: u64,
    pub idr_encoded_payload_bytes_received: u64,
    pub idr_frames_accepted: u64,
    pub idr_rtp_payload_bytes_accepted: u64,
    pub rtp_packets_dequeued: u64,
    pub rtp_payload_bytes_dequeued: u64,
    pub rtp_packets_write_succeeded: u64,
    pub rtp_payload_bytes_write_succeeded: u64,
    pub rtp_packets_write_failed: u64,
    pub rtp_payload_bytes_write_failed: u64,
    pub rtp_packets_write_skipped: u64,
    pub rtp_payload_bytes_write_skipped: u64,
    pub queue_wait_samples: u64,
    pub queue_wait_min_ms: f64,
    pub queue_wait_max_ms: f64,
    pub queue_wait_avg_ms: f64,
    pub rtp_write_latency_samples: u64,
    pub rtp_write_latency_min_ms: f64,
    pub rtp_write_latency_max_ms: f64,
    pub rtp_write_latency_avg_ms: f64,
}

impl VideoTransportMetrics {
    pub(crate) fn new(queue_capacity_frames: usize) -> Self {
        Self {
            queue_capacity_frames: queue_capacity_frames as u64,
            queue_depth_frames: AtomicU64::new(0),
            queue_max_depth_frames: AtomicU64::new(0),
            in_flight_frames: AtomicU64::new(0),
            in_flight_max_frames: AtomicU64::new(0),
            encoded_frames_received: AtomicU64::new(0),
            encoded_payload_bytes_received: AtomicU64::new(0),
            frames_accepted: AtomicU64::new(0),
            frames_rejected: AtomicU64::new(0),
            frames_replaced: AtomicU64::new(0),
            frames_cleared: AtomicU64::new(0),
            frames_dequeued: AtomicU64::new(0),
            idr_frames_received: AtomicU64::new(0),
            idr_encoded_payload_bytes_received: AtomicU64::new(0),
            idr_frames_accepted: AtomicU64::new(0),
            idr_rtp_payload_bytes_accepted: AtomicU64::new(0),
            rtp_packets_dequeued: AtomicU64::new(0),
            rtp_payload_bytes_dequeued: AtomicU64::new(0),
            rtp_packets_write_succeeded: AtomicU64::new(0),
            rtp_payload_bytes_write_succeeded: AtomicU64::new(0),
            rtp_packets_write_failed: AtomicU64::new(0),
            rtp_payload_bytes_write_failed: AtomicU64::new(0),
            rtp_packets_write_skipped: AtomicU64::new(0),
            rtp_payload_bytes_write_skipped: AtomicU64::new(0),
            queue_wait: Mutex::new(DurationAccumulator::default()),
            rtp_write_latency: Mutex::new(DurationAccumulator::default()),
            last_snapshot: Mutex::new(Instant::now()),
        }
    }

    pub(crate) fn record_encoded_frame(&self, encoded_bytes: usize, is_idr: bool) {
        self.encoded_frames_received.fetch_add(1, Ordering::Relaxed);
        self.encoded_payload_bytes_received
            .fetch_add(encoded_bytes as u64, Ordering::Relaxed);
        if is_idr {
            self.idr_frames_received.fetch_add(1, Ordering::Relaxed);
            self.idr_encoded_payload_bytes_received
                .fetch_add(encoded_bytes as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_enqueue(
        &self,
        accepted: bool,
        is_idr: bool,
        rtp_payload_bytes: usize,
        replaced_frames: usize,
        queue_depth: usize,
    ) {
        if accepted {
            self.frames_accepted.fetch_add(1, Ordering::Relaxed);
            if is_idr {
                self.idr_frames_accepted.fetch_add(1, Ordering::Relaxed);
                self.idr_rtp_payload_bytes_accepted
                    .fetch_add(rtp_payload_bytes as u64, Ordering::Relaxed);
            }
        } else {
            self.frames_rejected.fetch_add(1, Ordering::Relaxed);
        }
        self.frames_replaced
            .fetch_add(replaced_frames as u64, Ordering::Relaxed);
        self.set_queue_depth(queue_depth);
    }

    pub(crate) fn record_clear(&self, cleared_frames: usize, queue_depth: usize) {
        self.frames_cleared
            .fetch_add(cleared_frames as u64, Ordering::Relaxed);
        self.set_queue_depth(queue_depth);
    }

    pub(crate) fn record_dequeue(
        &self,
        rtp_packets: usize,
        rtp_payload_bytes: usize,
        queue_depth: usize,
        queue_wait: Duration,
    ) {
        self.frames_dequeued.fetch_add(1, Ordering::Relaxed);
        self.rtp_packets_dequeued
            .fetch_add(rtp_packets as u64, Ordering::Relaxed);
        self.rtp_payload_bytes_dequeued
            .fetch_add(rtp_payload_bytes as u64, Ordering::Relaxed);
        lock_unpoisoned(&self.queue_wait).record(queue_wait);
        self.set_queue_depth(queue_depth);
        let in_flight = self.in_flight_frames.fetch_add(1, Ordering::Relaxed) + 1;
        self.in_flight_max_frames
            .fetch_max(in_flight, Ordering::Relaxed);
    }

    pub(crate) fn record_write_succeeded(&self, rtp_payload_bytes: usize) {
        self.rtp_packets_write_succeeded
            .fetch_add(1, Ordering::Relaxed);
        self.rtp_payload_bytes_write_succeeded
            .fetch_add(rtp_payload_bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_write_failed(&self, rtp_payload_bytes: usize) {
        self.rtp_packets_write_failed
            .fetch_add(1, Ordering::Relaxed);
        self.rtp_payload_bytes_write_failed
            .fetch_add(rtp_payload_bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_write_skipped(&self, rtp_payload_bytes: usize) {
        self.rtp_packets_write_skipped
            .fetch_add(1, Ordering::Relaxed);
        self.rtp_payload_bytes_write_skipped
            .fetch_add(rtp_payload_bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_frame_write_finished(&self, write_latency: DurationAccumulator) {
        lock_unpoisoned(&self.rtp_write_latency).merge(write_latency);
        let previous = self.in_flight_frames.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous > 0, "finished a frame that was not in flight");
    }

    fn set_queue_depth(&self, queue_depth: usize) {
        let queue_depth = queue_depth as u64;
        self.queue_depth_frames
            .store(queue_depth, Ordering::Relaxed);
        self.queue_max_depth_frames
            .fetch_max(queue_depth, Ordering::Relaxed);
    }

    pub(crate) fn take_snapshot(&self) -> VideoTransportStats {
        let now = Instant::now();
        let interval_ms = {
            let mut last_snapshot = self
                .last_snapshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let interval_ms = now.duration_since(*last_snapshot).as_secs_f64() * 1000.0;
            *last_snapshot = now;
            interval_ms
        };

        let queue_depth_frames = self.queue_depth_frames.load(Ordering::Relaxed);
        let queue_max_depth_frames = self
            .queue_max_depth_frames
            .swap(queue_depth_frames, Ordering::Relaxed)
            .max(queue_depth_frames);
        let in_flight_frames = self.in_flight_frames.load(Ordering::Relaxed);
        let in_flight_max_frames = self
            .in_flight_max_frames
            .swap(in_flight_frames, Ordering::Relaxed)
            .max(in_flight_frames);
        let queue_wait = lock_unpoisoned(&self.queue_wait).take_snapshot();
        let rtp_write_latency = lock_unpoisoned(&self.rtp_write_latency).take_snapshot();
        let frames_rejected = self.frames_rejected.swap(0, Ordering::Relaxed);
        let frames_replaced = self.frames_replaced.swap(0, Ordering::Relaxed);
        let frames_cleared = self.frames_cleared.swap(0, Ordering::Relaxed);

        VideoTransportStats {
            interval_ms,
            queue_capacity_frames: self.queue_capacity_frames,
            queue_depth_frames,
            queue_max_depth_frames,
            in_flight_frames,
            in_flight_max_frames,
            encoded_frames_received: self.encoded_frames_received.swap(0, Ordering::Relaxed),
            encoded_payload_bytes_received: self
                .encoded_payload_bytes_received
                .swap(0, Ordering::Relaxed),
            frames_accepted: self.frames_accepted.swap(0, Ordering::Relaxed),
            frames_rejected,
            frames_replaced,
            frames_cleared,
            frames_dropped: frames_rejected + frames_replaced + frames_cleared,
            frames_dequeued: self.frames_dequeued.swap(0, Ordering::Relaxed),
            idr_frames_received: self.idr_frames_received.swap(0, Ordering::Relaxed),
            idr_encoded_payload_bytes_received: self
                .idr_encoded_payload_bytes_received
                .swap(0, Ordering::Relaxed),
            idr_frames_accepted: self.idr_frames_accepted.swap(0, Ordering::Relaxed),
            idr_rtp_payload_bytes_accepted: self
                .idr_rtp_payload_bytes_accepted
                .swap(0, Ordering::Relaxed),
            rtp_packets_dequeued: self.rtp_packets_dequeued.swap(0, Ordering::Relaxed),
            rtp_payload_bytes_dequeued: self.rtp_payload_bytes_dequeued.swap(0, Ordering::Relaxed),
            rtp_packets_write_succeeded: self
                .rtp_packets_write_succeeded
                .swap(0, Ordering::Relaxed),
            rtp_payload_bytes_write_succeeded: self
                .rtp_payload_bytes_write_succeeded
                .swap(0, Ordering::Relaxed),
            rtp_packets_write_failed: self.rtp_packets_write_failed.swap(0, Ordering::Relaxed),
            rtp_payload_bytes_write_failed: self
                .rtp_payload_bytes_write_failed
                .swap(0, Ordering::Relaxed),
            rtp_packets_write_skipped: self.rtp_packets_write_skipped.swap(0, Ordering::Relaxed),
            rtp_payload_bytes_write_skipped: self
                .rtp_payload_bytes_write_skipped
                .swap(0, Ordering::Relaxed),
            queue_wait_samples: queue_wait.samples,
            queue_wait_min_ms: queue_wait.min_ms,
            queue_wait_max_ms: queue_wait.max_ms,
            queue_wait_avg_ms: queue_wait.avg_ms,
            rtp_write_latency_samples: rtp_write_latency.samples,
            rtp_write_latency_min_ms: rtp_write_latency.min_ms,
            rtp_write_latency_max_ms: rtp_write_latency.max_ms,
            rtp_write_latency_avg_ms: rtp_write_latency.avg_ms,
        }
    }
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DurationAccumulator {
    samples: u64,
    total_ns: u64,
    min_ns: u64,
    max_ns: u64,
}

impl Default for DurationAccumulator {
    fn default() -> Self {
        Self {
            samples: 0,
            total_ns: 0,
            min_ns: u64::MAX,
            max_ns: 0,
        }
    }
}

impl DurationAccumulator {
    pub(crate) fn record(&mut self, duration: Duration) {
        let duration_ns = duration_ns(duration);
        self.samples = self.samples.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(duration_ns);
        self.min_ns = self.min_ns.min(duration_ns);
        self.max_ns = self.max_ns.max(duration_ns);
    }

    fn merge(&mut self, other: Self) {
        if other.samples == 0 {
            return;
        }
        self.samples = self.samples.saturating_add(other.samples);
        self.total_ns = self.total_ns.saturating_add(other.total_ns);
        self.min_ns = self.min_ns.min(other.min_ns);
        self.max_ns = self.max_ns.max(other.max_ns);
    }

    fn take_snapshot(&mut self) -> DurationSnapshot {
        let value = std::mem::take(self);
        if value.samples == 0 {
            return DurationSnapshot {
                samples: 0,
                min_ms: 0.0,
                max_ms: 0.0,
                avg_ms: 0.0,
            };
        }

        DurationSnapshot {
            samples: value.samples,
            min_ms: value.min_ns as f64 / 1_000_000.0,
            max_ms: value.max_ns as f64 / 1_000_000.0,
            avg_ms: value.total_ns as f64 / value.samples as f64 / 1_000_000.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DurationSnapshot {
    samples: u64,
    min_ms: f64,
    max_ms: f64,
    avg_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reports_and_resets_interval_counters() {
        let metrics = VideoTransportMetrics::new(2);
        metrics.record_encoded_frame(1_000, true);
        metrics.record_enqueue(true, true, 900, 0, 1);
        metrics.record_enqueue(true, false, 700, 0, 2);
        metrics.record_enqueue(false, false, 600, 0, 2);
        metrics.record_clear(1, 1);
        metrics.record_enqueue(true, true, 850, 1, 1);
        metrics.record_dequeue(3, 850, 0, Duration::from_millis(2));
        let mut write_latency = DurationAccumulator::default();
        metrics.record_write_succeeded(300);
        write_latency.record(Duration::from_micros(500));
        metrics.record_write_failed(200);
        write_latency.record(Duration::from_millis(1));
        metrics.record_write_skipped(350);

        let first = metrics.take_snapshot();
        assert_eq!(first.queue_capacity_frames, 2);
        assert_eq!(first.queue_depth_frames, 0);
        assert_eq!(first.queue_max_depth_frames, 2);
        assert_eq!(first.in_flight_frames, 1);
        assert_eq!(first.in_flight_max_frames, 1);
        assert_eq!(first.encoded_frames_received, 1);
        assert_eq!(first.encoded_payload_bytes_received, 1_000);
        assert_eq!(first.frames_accepted, 3);
        assert_eq!(first.frames_rejected, 1);
        assert_eq!(first.frames_replaced, 1);
        assert_eq!(first.frames_cleared, 1);
        assert_eq!(first.frames_dropped, 3);
        assert_eq!(first.frames_dequeued, 1);
        assert_eq!(first.idr_frames_received, 1);
        assert_eq!(first.idr_encoded_payload_bytes_received, 1_000);
        assert_eq!(first.idr_frames_accepted, 2);
        assert_eq!(first.idr_rtp_payload_bytes_accepted, 1_750);
        assert_eq!(first.rtp_packets_dequeued, 3);
        assert_eq!(first.rtp_payload_bytes_dequeued, 850);
        assert_eq!(first.rtp_packets_write_succeeded, 1);
        assert_eq!(first.rtp_payload_bytes_write_succeeded, 300);
        assert_eq!(first.rtp_packets_write_failed, 1);
        assert_eq!(first.rtp_payload_bytes_write_failed, 200);
        assert_eq!(first.rtp_packets_write_skipped, 1);
        assert_eq!(first.rtp_payload_bytes_write_skipped, 350);
        assert_eq!(first.queue_wait_samples, 1);
        assert_eq!(first.queue_wait_min_ms, 2.0);
        assert_eq!(first.queue_wait_max_ms, 2.0);
        assert_eq!(first.queue_wait_avg_ms, 2.0);
        // Latency is committed as one internally consistent batch when the
        // in-flight frame finishes, not packet-by-packet during this snapshot.
        assert_eq!(first.rtp_write_latency_samples, 0);

        metrics.record_frame_write_finished(write_latency);

        let second = metrics.take_snapshot();
        assert_eq!(second.queue_depth_frames, 0);
        assert_eq!(second.queue_max_depth_frames, 0);
        assert_eq!(second.in_flight_frames, 0);
        // The frame was still in flight at the start of this interval.
        assert_eq!(second.in_flight_max_frames, 1);
        assert_eq!(second.encoded_frames_received, 0);
        assert_eq!(second.frames_accepted, 0);
        assert_eq!(second.frames_dropped, 0);
        assert_eq!(second.rtp_payload_bytes_dequeued, 0);
        assert_eq!(second.rtp_packets_write_succeeded, 0);
        assert_eq!(second.rtp_packets_write_failed, 0);
        assert_eq!(second.queue_wait_samples, 0);
        assert_eq!(second.queue_wait_avg_ms, 0.0);
        assert_eq!(second.rtp_write_latency_samples, 2);
        assert_eq!(second.rtp_write_latency_min_ms, 0.5);
        assert_eq!(second.rtp_write_latency_max_ms, 1.0);
        assert_eq!(second.rtp_write_latency_avg_ms, 0.75);
    }

    #[test]
    fn duration_nanoseconds_saturate_instead_of_wrapping() {
        assert_eq!(duration_ns(Duration::MAX), u64::MAX);
    }
}
