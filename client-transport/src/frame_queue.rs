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

    pub fn len(&self) -> usize {
        self.lock().frames.len()
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
}
