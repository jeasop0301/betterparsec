//! Bounded-queue frame pacing (G020). At high refresh (4K120/144) the present
//! path must never accumulate unbounded latency: the pending-frame queue is
//! hard-bounded, and frames older than a small multiple of the target frame
//! interval are dropped as stale rather than displayed late. This pure policy
//! computes the target interval from the negotiated refresh, bounds the queue,
//! drops stale frames, and exposes queue-age / drop / present telemetry.
//!
//! Foundation module: the live wiring into the D3D11 present loop
//! (app-native/src/present.rs, video.rs) and the physical 4K120/144 + VRR /
//! waitable / tearing campaign are the physically-gated part of G020. This state
//! machine is pure and headless unit-tested; `#![allow(dead_code)]` matches the
//! repo's deferred-wire-in pattern.
#![allow(dead_code)]

use std::collections::VecDeque;

/// Frame-pacing policy with a hard-bounded pending queue and stale-frame drop.
#[derive(Debug, Clone)]
pub struct FramePacer {
    target_interval_us: u64,
    max_queue_age_us: u64,
    max_queue_frames: usize,
    queue: VecDeque<u64>,
    dropped_stale: u64,
    presented: u64,
}

impl FramePacer {
    /// A pacer for a negotiated refresh (`refresh_mhz` is millihertz, e.g.
    /// `144_000` = 144 Hz) that keeps at most `max_queue_frames` pending and
    /// drops anything older than that many frame intervals.
    pub fn new(refresh_mhz: u32, max_queue_frames: usize) -> Self {
        let refresh_mhz = refresh_mhz.max(1);
        let target_interval_us = 1_000_000_000u64 / u64::from(refresh_mhz);
        let frames = max_queue_frames.max(1);
        Self {
            target_interval_us,
            max_queue_age_us: target_interval_us.saturating_mul(frames as u64),
            max_queue_frames: frames,
            queue: VecDeque::new(),
            dropped_stale: 0,
            presented: 0,
        }
    }

    /// The target frame interval in microseconds.
    pub fn target_interval_us(&self) -> u64 {
        self.target_interval_us
    }

    /// Frames dropped as stale (queue overflow or over-age).
    pub fn dropped_stale(&self) -> u64 {
        self.dropped_stale
    }

    /// Frames presented.
    pub fn presented(&self) -> u64 {
        self.presented
    }

    /// Current pending-queue depth (never exceeds `max_queue_frames`).
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Age (µs) of the oldest pending frame at `now_us`, or 0 when empty.
    pub fn queue_age_us(&self, now_us: u64) -> u64 {
        self.queue.front().map_or(0, |&f| now_us.saturating_sub(f))
    }

    /// Enqueue a newly-arrived frame (its arrival timestamp, µs). If the bounded
    /// queue is full the oldest pending frame is dropped as stale — this is the
    /// no-unbounded-latency guarantee.
    pub fn push(&mut self, arrival_us: u64) {
        while self.queue.len() >= self.max_queue_frames {
            self.queue.pop_front();
            self.dropped_stale += 1;
        }
        self.queue.push_back(arrival_us);
    }

    /// Pick the frame to present at `now_us`: first drop every frame older than
    /// the max queue age (stale), then present the oldest remaining. Returns its
    /// arrival timestamp, or `None` when the queue is empty after dropping.
    pub fn pick_present(&mut self, now_us: u64) -> Option<u64> {
        while let Some(&front) = self.queue.front() {
            if now_us.saturating_sub(front) > self.max_queue_age_us {
                self.queue.pop_front();
                self.dropped_stale += 1;
            } else {
                break;
            }
        }
        let picked = self.queue.pop_front();
        if picked.is_some() {
            self.presented += 1;
        }
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_derives_from_refresh() {
        assert_eq!(FramePacer::new(60_000, 2).target_interval_us(), 16_666);
        assert_eq!(FramePacer::new(144_000, 2).target_interval_us(), 6_944);
        // Degenerate refresh does not divide by zero.
        assert!(FramePacer::new(0, 1).target_interval_us() > 0);
    }

    #[test]
    fn queue_is_hard_bounded_no_unbounded_latency() {
        let mut pacer = FramePacer::new(60_000, 2);
        for t in 0..10u64 {
            pacer.push(t * 1000);
            // The queue never grows past the bound, however many frames arrive.
            assert!(pacer.queue_len() <= 2);
        }
        // 10 pushed, at most 2 retained -> 8 dropped as stale.
        assert_eq!(pacer.dropped_stale(), 8);
        assert_eq!(pacer.queue_len(), 2);
    }

    #[test]
    fn stale_frames_are_dropped_before_present() {
        // 60 Hz, keep 1 frame: max age = one 16_666 µs interval.
        let mut pacer = FramePacer::new(60_000, 1);
        pacer.push(0);
        // Present far in the future: the frame is stale and dropped, nothing left.
        assert_eq!(pacer.pick_present(20_000), None);
        assert_eq!(pacer.dropped_stale(), 1);
        assert_eq!(pacer.presented(), 0);
        // A fresh frame within the age bound is presented.
        pacer.push(10_000);
        assert_eq!(pacer.pick_present(15_000), Some(10_000));
        assert_eq!(pacer.presented(), 1);
    }

    #[test]
    fn presents_oldest_within_bound_first() {
        let mut pacer = FramePacer::new(144_000, 4);
        pacer.push(1_000);
        pacer.push(2_000);
        // Both within the age bound at now=3_000; the oldest presents first.
        assert_eq!(pacer.pick_present(3_000), Some(1_000));
        assert_eq!(pacer.pick_present(3_000), Some(2_000));
        assert_eq!(pacer.pick_present(3_000), None);
    }

    #[test]
    fn queue_age_reports_oldest_pending() {
        let mut pacer = FramePacer::new(60_000, 4);
        assert_eq!(pacer.queue_age_us(1_000), 0);
        pacer.push(1_000);
        assert_eq!(pacer.queue_age_us(5_000), 4_000);
    }
}
