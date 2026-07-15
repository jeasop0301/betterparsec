//! Bounded blocking frame queue — the native client's replacement for
//! moonlight-common-c's `LiWaitForNextVideoFrame` pull loop
//! (m6-native-spike.md §B-1 Option-3, ffmpeg.cpp pull-renderer thread).
//!
//! Producer: the transport receive thread pushing reassembled
//! [`DecodeUnit`]s. Consumer: the FFmpeg decoder thread blocking in
//! [`FrameQueue::wait_pop`].

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use transport_core::video_rx::DecodeUnit;

/// Default frame capacity. At 60 fps this is ~266 ms of backlog — if the
/// decoder falls further behind, frames are stale and resync via IDR is
/// cheaper than draining (same policy family as the receiver's pending cap).
pub const DEFAULT_FRAME_CAP: usize = 16;

#[derive(Debug, Default)]
struct QueueState {
    frames: VecDeque<DecodeUnit>,
    closed: bool,
    /// Latched when an overflow flushed the queue; consumed by
    /// [`FrameQueue::take_overflowed`] → caller sends NeedsIdr.
    overflowed: bool,
}

/// Thread-safe bounded FIFO of decode units with blocking pop.
#[derive(Debug)]
pub struct FrameQueue {
    state: Mutex<QueueState>,
    cond: Condvar,
    cap: usize,
}

impl FrameQueue {
    pub fn new(cap: usize) -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            cond: Condvar::new(),
            cap: cap.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        // A panicking peer must not deadlock the decoder thread; queue state
        // stays structurally valid under poisoning.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Push one frame. Returns `false` when the queue is closed.
    ///
    /// Overflow policy: when full, the entire backlog is dropped (stale — the
    /// bitstream is broken anyway once we skip delta frames), `overflowed` is
    /// latched for an IDR request, and the new frame is enqueued.
    pub fn push(&self, unit: DecodeUnit) -> bool {
        let mut st = self.lock();
        if st.closed {
            return false;
        }
        if st.frames.len() >= self.cap {
            st.frames.clear();
            st.overflowed = true;
        }
        st.frames.push_back(unit);
        drop(st);
        self.cond.notify_one();
        true
    }

    /// Block up to `timeout` for the next frame. `None` on timeout or when
    /// the queue is closed and drained.
    pub fn wait_pop(&self, timeout: Duration) -> Option<DecodeUnit> {
        let mut st = self.lock();
        loop {
            if let Some(unit) = st.frames.pop_front() {
                return Some(unit);
            }
            if st.closed {
                return None;
            }
            let (guard, res) = self
                .cond
                .wait_timeout(st, timeout)
                .unwrap_or_else(PoisonError::into_inner);
            st = guard;
            if res.timed_out() && st.frames.is_empty() {
                return None;
            }
        }
    }

    /// Close the queue and wake all waiters. Queued frames stay poppable;
    /// further pushes are rejected.
    pub fn close(&self) {
        self.lock().closed = true;
        self.cond.notify_all();
    }

    /// Consume the overflow latch (true at most once per overflow burst).
    pub fn take_overflowed(&self) -> bool {
        std::mem::take(&mut self.lock().overflowed)
    }

    /// Non-blocking pop of the front frame, if one is queued. Never blocks;
    /// used to drain stale backlog so only the newest picture is presented.
    pub fn try_pop(&self) -> Option<DecodeUnit> {
        self.lock().frames.pop_front()
    }

    pub fn len(&self) -> usize {
        self.lock().frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Default audio packet capacity. Opus packets arrive every 5–20 ms; 64
/// packets is 320 ms–1.3 s of backlog — far more than the render buffer
/// ever wants, so overflow only happens when the consumer stalls.
pub const DEFAULT_AUDIO_CAP: usize = 64;

#[derive(Debug, Default)]
struct SampleState {
    packets: VecDeque<Vec<u8>>,
    closed: bool,
}

/// Thread-safe bounded FIFO of opaque audio packets (opus) with blocking
/// pop. Unlike [`FrameQueue`], overflow drops the *oldest* packet: audio
/// has no reference chain, and skipping ahead preserves latency at the
/// cost of one inaudible gap.
#[derive(Debug)]
pub struct SampleQueue {
    state: Mutex<SampleState>,
    cond: Condvar,
    cap: usize,
}

impl SampleQueue {
    pub fn new(cap: usize) -> Self {
        Self {
            state: Mutex::new(SampleState::default()),
            cond: Condvar::new(),
            cap: cap.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SampleState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Push one packet. Returns `false` when the queue is closed.
    pub fn push(&self, pkt: Vec<u8>) -> bool {
        let mut st = self.lock();
        if st.closed {
            return false;
        }
        while st.packets.len() >= self.cap {
            st.packets.pop_front();
        }
        st.packets.push_back(pkt);
        drop(st);
        self.cond.notify_one();
        true
    }

    /// Block up to `timeout` for the next packet. `None` on timeout or
    /// when the queue is closed and drained.
    pub fn wait_pop(&self, timeout: Duration) -> Option<Vec<u8>> {
        let mut st = self.lock();
        loop {
            if let Some(pkt) = st.packets.pop_front() {
                return Some(pkt);
            }
            if st.closed {
                return None;
            }
            let (guard, res) = self
                .cond
                .wait_timeout(st, timeout)
                .unwrap_or_else(PoisonError::into_inner);
            st = guard;
            if res.timed_out() && st.packets.is_empty() {
                return None;
            }
        }
    }

    /// Close the queue and wake all waiters. Queued packets stay poppable;
    /// further pushes are rejected.
    pub fn close(&self) {
        self.lock().closed = true;
        self.cond.notify_all();
    }

    pub fn len(&self) -> usize {
        self.lock().packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn unit(frame_id: u32) -> DecodeUnit {
        DecodeUnit {
            frame_id,
            is_key: frame_id == 0,
            timestamp_us: frame_id * 16_667,
            duration_us: 16_667,
            data: vec![frame_id as u8; 8],
        }
    }

    #[test]
    fn fifo_order_preserved() {
        let q = FrameQueue::new(8);
        for i in 0..5 {
            assert!(q.push(unit(i)));
        }
        for i in 0..5 {
            let u = q
                .wait_pop(Duration::from_millis(10))
                .expect("frame present");
            assert_eq!(u.frame_id, i);
        }
        assert!(q.is_empty());
    }

    #[test]
    fn wait_pop_times_out_when_empty() {
        let q = FrameQueue::new(8);
        let start = Instant::now();
        assert!(q.wait_pop(Duration::from_millis(30)).is_none());
        assert!(
            start.elapsed() >= Duration::from_millis(25),
            "actually waited"
        );
    }

    #[test]
    fn overflow_flushes_backlog_and_latches_idr() {
        let q = FrameQueue::new(4);
        for i in 0..4 {
            assert!(q.push(unit(i)));
        }
        assert!(!q.take_overflowed(), "no overflow at exactly cap");

        assert!(q.push(unit(4)), "overflow push still accepted");
        assert_eq!(q.len(), 1, "backlog flushed, only newest frame kept");
        assert!(q.take_overflowed(), "overflow latched");
        assert!(!q.take_overflowed(), "latch consumed");

        let u = q.wait_pop(Duration::from_millis(10)).expect("newest frame");
        assert_eq!(u.frame_id, 4);
    }

    #[test]
    fn close_unblocks_waiter_and_rejects_pushes() {
        let q = Arc::new(FrameQueue::new(8));
        let q2 = Arc::clone(&q);
        let waiter = std::thread::spawn(move || q2.wait_pop(Duration::from_secs(30)));

        // Give the waiter a moment to block, then close.
        std::thread::sleep(Duration::from_millis(20));
        q.close();
        let joined = waiter.join().expect("waiter thread must not panic");
        assert!(joined.is_none(), "closed + empty → None");

        assert!(!q.push(unit(0)), "push after close rejected");
    }

    #[test]
    fn close_keeps_queued_frames_poppable() {
        let q = FrameQueue::new(8);
        assert!(q.push(unit(7)));
        q.close();
        let u = q
            .wait_pop(Duration::from_millis(10))
            .expect("drain after close");
        assert_eq!(u.frame_id, 7);
        assert!(q.wait_pop(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn producer_consumer_threads_hand_over_all_frames() {
        // Cap above the total so the overflow-flush policy (tested separately)
        // cannot trigger: this test pins pure handover correctness.
        let q = Arc::new(FrameQueue::new(256));
        let producer = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                for i in 0..200 {
                    assert!(q.push(unit(i)));
                    if i % 32 == 0 {
                        std::thread::yield_now();
                    }
                }
            })
        };
        let consumer = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                let mut got = Vec::new();
                while got.len() < 200 {
                    if let Some(u) = q.wait_pop(Duration::from_secs(5)) {
                        got.push(u.frame_id);
                    } else {
                        break;
                    }
                }
                got
            })
        };
        producer.join().expect("producer ok");
        let got = consumer.join().expect("consumer ok");
        assert_eq!(got.len(), 200, "every frame handed over exactly once");
        assert!(
            got.windows(2).all(|w| w[0] < w[1]),
            "strictly increasing order"
        );
    }

    // ── SampleQueue (audio) ────────────────────────────────────────────

    #[test]
    fn sample_queue_fifo_and_timeout() {
        let q = SampleQueue::new(8);
        assert!(q.push(vec![1]));
        assert!(q.push(vec![2, 2]));
        assert_eq!(q.wait_pop(Duration::from_millis(10)), Some(vec![1]));
        assert_eq!(q.wait_pop(Duration::from_millis(10)), Some(vec![2, 2]));
        let t = Instant::now();
        assert_eq!(q.wait_pop(Duration::from_millis(30)), None);
        assert!(
            t.elapsed() >= Duration::from_millis(25),
            "timed out, not spun"
        );
    }

    #[test]
    fn sample_queue_overflow_drops_oldest_only() {
        let q = SampleQueue::new(3);
        for i in 0..5u8 {
            assert!(q.push(vec![i]));
        }
        // 0 and 1 dropped; 2, 3, 4 remain in order.
        assert_eq!(q.len(), 3);
        assert_eq!(q.wait_pop(Duration::ZERO), Some(vec![2]));
        assert_eq!(q.wait_pop(Duration::ZERO), Some(vec![3]));
        assert_eq!(q.wait_pop(Duration::ZERO), Some(vec![4]));
    }

    #[test]
    fn sample_queue_close_rejects_push_and_drains() {
        let q = SampleQueue::new(4);
        assert!(q.push(vec![7]));
        q.close();
        assert!(!q.push(vec![8]));
        // Queued packet still poppable, then None immediately (no timeout).
        assert_eq!(q.wait_pop(Duration::from_secs(5)), Some(vec![7]));
        let t = Instant::now();
        assert_eq!(q.wait_pop(Duration::from_secs(5)), None);
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "closed pop returns fast"
        );
    }

    #[test]
    fn sample_queue_close_wakes_blocked_waiter() {
        let q = Arc::new(SampleQueue::new(4));
        let waiter = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || q.wait_pop(Duration::from_secs(30)))
        };
        std::thread::sleep(Duration::from_millis(50));
        q.close();
        let t = Instant::now();
        assert_eq!(waiter.join().expect("join"), None);
        assert!(t.elapsed() < Duration::from_secs(5), "woken by close");
    }
}
