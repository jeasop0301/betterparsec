//! Bounded blocking frame/discontinuity queue — the native client's
//! replacement for moonlight-common-c's `LiWaitForNextVideoFrame` pull loop
//! (m6-native-spike.md §B-1 Option-3, ffmpeg.cpp pull-renderer thread).
//!
//! Producer: the transport receive thread pushing reassembled
//! [`DecodeUnit`]s and [`RxEvent::Discontinuity`](transport_core::video_rx::RxEvent::Discontinuity)
//! events, in wire order. Consumer: the FFmpeg decoder thread blocking in
//! [`FrameQueue::wait_event`] (or the frame-only compatibility
//! [`FrameQueue::wait_pop`]).
//!
//! G002 contract: every discontinuity — transport-sourced or queue-overflow
//! — is queued as a first-class [`VideoEvent`] ahead of any frame it gates,
//! so a consumer draining the queue in order can never observe a frame
//! before the reset event that must flush the decoder first.
//!
//! Decoder-recovery (the flush → decode-a-fresh-key handshake) is keyed by a
//! monotonic `generation`, not by epoch: most discontinuity reasons do not
//! change the epoch, so two discontinuities can share one epoch and a stale
//! ack for the first must never close the second's recovery. Recovery is
//! opened *inside* the same lock that publishes the gating event (see
//! [`FrameQueue::push_discontinuity`]/[`FrameQueue::push_frame_reporting_overflow`]),
//! so a consumer that pops the event can never observe recovery still
//! closed/never-opened.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use transport_core::video_rx::{DecodeUnit, DiscontinuityReason};

/// Default frame capacity. At 60 fps this is ~266 ms of backlog — if the
/// decoder falls further behind, frames are stale and resync via IDR is
/// cheaper than draining (same policy family as the receiver's pending cap).
pub const DEFAULT_FRAME_CAP: usize = 16;

/// One ordered item in a [`FrameQueue`]: a reassembled frame, or a
/// discontinuity that must flush/reset the decoder before any later frame
/// is presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoEvent {
    Frame(DecodeUnit),
    Discontinuity {
        /// Monotonic identity of the decoder-recovery this discontinuity
        /// opened — see [`QueueState::recovery`]. Two discontinuities can
        /// share `epoch` (only [`DiscontinuityReason::EpochTransition`]
        /// changes it), so `generation` — not `epoch` — is the ack key.
        generation: u64,
        epoch: u32,
        reason: DiscontinuityReason,
    },
}

impl VideoEvent {
    pub fn as_frame(&self) -> Option<&DecodeUnit> {
        match self {
            VideoEvent::Frame(unit) => Some(unit),
            VideoEvent::Discontinuity { .. } => None,
        }
    }
}

#[derive(Debug, Default)]
struct QueueState {
    events: VecDeque<VideoEvent>,
    closed: bool,
    /// Latched when a frame-push overflow flushed the queue; consumed by
    /// [`FrameQueue::take_overflowed`] → caller sends NeedsIdr. The
    /// [`DiscontinuityReason::QueueOverflow`] event itself is the
    /// decoder-recovery signal; this flag stays for the pre-existing
    /// receiver-loss-style IDR latch callers already poll. Also reused (see
    /// [`FrameQueue::push_discontinuity`]) to record a non-empty backlog
    /// destroyed by a discontinuity arriving at capacity — that path never
    /// enqueues a second reset event, so this latch is the only trace of
    /// the drop.
    overflowed: bool,
    /// Monotonic counter: bumped every time a recovery opens (every
    /// discontinuity/overflow event), so each opened recovery — even ones
    /// sharing an epoch — gets a distinct identity. `0` is never assigned
    /// (mirrors [`crate::capi::SessionGeneration`]'s wrap-avoiding-zero
    /// pattern), so `Some(0)` can never collide with "no recovery opened
    /// yet" bookkeeping elsewhere.
    next_generation: u64,
    /// Currently open decoder-recovery: `(generation, epoch)`, or `None`.
    /// Lives inside this same lock so opening it is atomic with the
    /// discontinuity/overflow event that gates it becoming poppable — a
    /// consumer that pops that event can never observe recovery as
    /// closed/never-opened (the TOCTOU the un-generationed design had).
    /// Keyed by generation, not epoch alone: most discontinuity reasons
    /// don't change the epoch, so an ack for an earlier same-epoch
    /// recovery must never close a later one.
    recovery: Option<(u64, u32)>,
}

/// Opens a fresh decoder-recovery generation for `epoch`, called under
/// `st`'s own lock so it is atomic with whatever push makes the gating
/// event poppable. Returns the assigned generation.
fn open_recovery_locked(st: &mut QueueState, epoch: u32) -> u64 {
    st.next_generation = st.next_generation.wrapping_add(1).max(1);
    let generation = st.next_generation;
    st.recovery = Some((generation, epoch));
    generation
}

/// Outcome of [`FrameQueue::push_frame_reporting_overflow`].
#[derive(Debug, Clone, Copy)]
pub struct FramePush {
    /// This push's backlog was full and got flushed into a fresh
    /// [`DiscontinuityReason::QueueOverflow`] reset ahead of the frame.
    pub overflowed: bool,
    /// Generation of the recovery opened for the overflow reset, when
    /// `overflowed`. `None` when this push did not overflow.
    pub generation: Option<u64>,
}

/// Thread-safe bounded FIFO of ordered video events (frames and
/// discontinuities) with blocking pop.
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
    /// Overflow policy: when full, the entire backlog is atomically cleared
    /// (stale — the bitstream is broken anyway once we skip delta frames), a
    /// [`DiscontinuityReason::QueueOverflow`] discontinuity is enqueued
    /// *before* the new frame (so a consumer draining in order flushes/resets
    /// before it ever sees this frame), `overflowed` latches for an IDR
    /// request, and the new frame is enqueued after its reset event.
    pub fn push_frame(&self, unit: DecodeUnit) -> bool {
        self.push_frame_reporting_overflow(unit).is_some()
    }

    /// Same push as [`push_frame`](Self::push_frame), but also reports
    /// whether *this* push triggered a queue-overflow reset — for callers
    /// that must react immediately (e.g. opening decoder-recovery state)
    /// without consuming the separate [`take_overflowed`](Self::take_overflowed)
    /// IDR latch, which stays a one-shot-per-burst signal for its existing
    /// pollers. `None` when the queue is closed (nothing was pushed).
    pub fn push_frame_reporting_overflow(&self, unit: DecodeUnit) -> Option<FramePush> {
        let mut st = self.lock();
        if st.closed {
            return None;
        }
        let mut overflowed = false;
        let mut generation = None;
        if st.events.len() >= self.cap {
            let epoch = unit.epoch;
            st.events.clear();
            // Recovery opens under this same lock, atomically with the
            // reset event below becoming poppable.
            let opened = open_recovery_locked(&mut st, epoch);
            st.events.push_back(VideoEvent::Discontinuity {
                generation: opened,
                epoch,
                reason: DiscontinuityReason::QueueOverflow,
            });
            st.overflowed = true;
            overflowed = true;
            generation = Some(opened);
        }
        st.events.push_back(VideoEvent::Frame(unit));
        drop(st);
        self.cond.notify_one();
        Some(FramePush {
            overflowed,
            generation,
        })
    }

    /// Push one discontinuity (transport-sourced: epoch transition, reorder
    /// gap, reassembly/FEC eviction, metadata/CRC mismatch, memory cap).
    /// Opens a fresh decoder-recovery generation for `epoch`, atomically
    /// with the event becoming poppable, and returns it. `None` when the
    /// queue is closed (nothing was pushed, no recovery opened).
    ///
    /// Every discontinuity must reach the queue before any later frame, so
    /// this never silently drops the event: at capacity the stale backlog is
    /// cleared first (same policy as [`push_frame`](Self::push_frame)),
    /// exactly like an overflow, then the discontinuity is enqueued. Unlike
    /// the frame-overflow path, no second [`DiscontinuityReason::QueueOverflow`]
    /// event is enqueued here — the discontinuity we are about to push *is*
    /// the reset the consumer needs, so a second one would be redundant —
    /// but the backlog it just destroyed (which may have held other
    /// not-yet-popped events, including older discontinuities) is real data
    /// loss, so it latches `overflowed` exactly like [`push_frame`]'s own
    /// overflow does, for the existing IDR-request pollers.
    pub fn push_discontinuity(&self, epoch: u32, reason: DiscontinuityReason) -> Option<u64> {
        let mut st = self.lock();
        if st.closed {
            return None;
        }
        if st.events.len() >= self.cap {
            st.overflowed = true;
            st.events.clear();
        }
        let generation = open_recovery_locked(&mut st, epoch);
        st.events.push_back(VideoEvent::Discontinuity {
            generation,
            epoch,
            reason,
        });
        drop(st);
        self.cond.notify_one();
        Some(generation)
    }

    /// Block up to `timeout` for the next event (frame or discontinuity).
    /// `None` on timeout or when the queue is closed and drained. Computes
    /// one deadline up front and waits on the remainder each iteration, so a
    /// spurious/raced wakeup can't re-arm the full timeout and overshoot the
    /// caller's requested bound.
    pub fn wait_event(&self, timeout: Duration) -> Option<VideoEvent> {
        let deadline = std::time::Instant::now() + timeout;
        let mut st = self.lock();
        loop {
            if let Some(ev) = st.events.pop_front() {
                return Some(ev);
            }
            if st.closed {
                return None;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let (guard, res) = self
                .cond
                .wait_timeout(st, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            st = guard;
            if (res.timed_out() || remaining.is_zero()) && st.events.is_empty() {
                return None;
            }
        }
    }

    /// Non-blocking pop of the next event, if one is queued. Never blocks.
    pub fn try_event(&self) -> Option<VideoEvent> {
        self.lock().events.pop_front()
    }

    /// Block up to `timeout` for the next frame — the
    /// `LiWaitForNextVideoFrame` replacement. `None` on timeout or when the
    /// queue is closed and drained.
    ///
    /// Compatibility shim for consumers that only understand frames (probes,
    /// the pre-G002 decoder pull loop): discontinuities are transparently
    /// consumed and skipped in order, never exposing a frame ahead of a reset
    /// event still pending behind it — they are simply not surfaced to this
    /// caller. Prefer [`wait_event`](Self::wait_event) for decoder-recovery
    /// aware consumers.
    pub fn wait_pop(&self, timeout: Duration) -> Option<DecodeUnit> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match self.wait_event(remaining)? {
                VideoEvent::Frame(unit) => return Some(unit),
                VideoEvent::Discontinuity { .. } => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    continue;
                }
            }
        }
    }

    /// Close the queue and wake all waiters. Queued events stay poppable;
    /// further pushes are rejected.
    pub fn close(&self) {
        self.lock().closed = true;
        self.cond.notify_all();
    }

    /// Consume the overflow latch (true at most once per overflow burst).
    pub fn take_overflowed(&self) -> bool {
        std::mem::take(&mut self.lock().overflowed)
    }
    /// Currently open decoder-recovery: `(generation, epoch)`, or `None`
    /// when the decoder has no outstanding flush/decoded-key handshake.
    pub fn recovery(&self) -> Option<(u64, u32)> {
        self.lock().recovery
    }

    /// Close decoder-recovery iff it is currently open for exactly this
    /// `(generation, epoch)` pair. A stale ack (generation superseded by a
    /// later discontinuity, even one sharing the same epoch) or a
    /// wrong-epoch ack is a no-op. Returns whether this call closed it.
    pub fn acknowledge_recovery(&self, generation: u64, epoch: u32) -> bool {
        let mut st = self.lock();
        if st.recovery == Some((generation, epoch)) {
            st.recovery = None;
            true
        } else {
            false
        }
    }

    /// Unconditionally clear any open recovery (a fresh generation/session
    /// must not inherit stale recovery state).
    pub fn clear_recovery(&self) {
        self.lock().recovery = None;
    }

    /// Non-blocking pop of the front frame, if one is queued. Never blocks;
    /// used to drain stale backlog so only the newest picture is presented.
    /// Compatibility shim: discontinuities in front are consumed and skipped.
    pub fn try_pop(&self) -> Option<DecodeUnit> {
        loop {
            match self.try_event()? {
                VideoEvent::Frame(unit) => return Some(unit),
                VideoEvent::Discontinuity { .. } => continue,
            }
        }
    }

    pub fn len(&self) -> usize {
        self.lock().events.len()
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
            epoch: 0,
            is_key: frame_id == 0,
            timestamp_us: frame_id * 16_667,
            duration_us: 16_667,
            data: vec![frame_id as u8; 8],
        }
    }

    fn unit_epoch(frame_id: u32, epoch: u32) -> DecodeUnit {
        DecodeUnit {
            epoch,
            ..unit(frame_id)
        }
    }

    #[test]
    fn fifo_order_preserved() {
        let q = FrameQueue::new(8);
        for i in 0..5 {
            assert!(q.push_frame(unit(i)));
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
            assert!(q.push_frame(unit(i)));
        }
        assert!(!q.take_overflowed(), "no overflow at exactly cap");

        assert!(
            q.push_frame(unit_epoch(4, 9)),
            "overflow push still accepted"
        );
        assert_eq!(
            q.len(),
            2,
            "backlog flushed, only the QueueOverflow reset + newest frame remain"
        );
        assert!(q.take_overflowed(), "overflow latched");
        assert!(!q.take_overflowed(), "latch consumed");
        assert_eq!(
            q.recovery(),
            Some((1, 9)),
            "overflow opens recovery for the triggering frame's epoch, generation 1"
        );

        // Reset-before-frame: the overflow discontinuity is queued strictly
        // ahead of the frame that triggered it.
        assert_eq!(
            q.try_event(),
            Some(VideoEvent::Discontinuity {
                generation: 1,
                epoch: 9,
                reason: DiscontinuityReason::QueueOverflow,
            }),
            "overflow reset must be visible before the retained frame"
        );
        let u = q.wait_pop(Duration::from_millis(10)).expect("newest frame");
        assert_eq!(u.frame_id, 4);
    }

    #[test]
    fn overflow_recovery_generation_is_reported_by_push_frame_reporting_overflow() {
        // The caller (RxCore) needs the assigned generation synchronously
        // from the same call that pushed the overflow reset — round-tripping
        // through a separate query would reopen the TOCTOU window this
        // return value exists to close.
        let q = FrameQueue::new(2);
        assert!(q.push_frame(unit(0)));
        assert!(q.push_frame(unit(1)));
        let push = q
            .push_frame_reporting_overflow(unit_epoch(2, 5))
            .expect("queue open");
        assert!(push.overflowed);
        assert_eq!(push.generation, Some(1));
        assert_eq!(q.recovery(), Some((1, 5)));
    }

    #[test]
    fn wait_pop_compat_skips_discontinuities_but_never_reorders() {
        let q = FrameQueue::new(8);
        assert!(
            q.push_discontinuity(1, DiscontinuityReason::EpochTransition)
                .is_some()
        );
        assert!(q.push_frame(unit(0)));
        // Frame-only compatibility consumers never see the discontinuity, but
        // it is still consumed from the front of the queue before the frame.
        let u = q
            .wait_pop(Duration::from_millis(10))
            .expect("frame present");
        assert_eq!(u.frame_id, 0);
        assert!(q.is_empty());
    }

    #[test]
    fn multiple_discontinuities_stay_ordered_with_frames() {
        let q = FrameQueue::new(8);
        let gen1 = q
            .push_discontinuity(1, DiscontinuityReason::EpochTransition)
            .expect("queue open");
        assert!(q.push_frame(unit_epoch(0, 1)));
        let gen2 = q
            .push_discontinuity(2, DiscontinuityReason::ReorderGap)
            .expect("queue open");
        assert!(q.push_frame(unit_epoch(1, 2)));
        assert_ne!(gen1, gen2, "each opened recovery gets a fresh generation");

        assert_eq!(
            q.try_event(),
            Some(VideoEvent::Discontinuity {
                generation: gen1,
                epoch: 1,
                reason: DiscontinuityReason::EpochTransition,
            })
        );
        assert_eq!(
            q.try_event()
                .and_then(|e| e.as_frame().cloned())
                .map(|u| u.frame_id),
            Some(0)
        );
        assert_eq!(
            q.try_event(),
            Some(VideoEvent::Discontinuity {
                generation: gen2,
                epoch: 2,
                reason: DiscontinuityReason::ReorderGap,
            })
        );
        assert_eq!(
            q.try_event()
                .and_then(|e| e.as_frame().cloned())
                .map(|u| u.frame_id),
            Some(1)
        );
        assert!(q.try_event().is_none());
    }

    #[test]
    fn same_epoch_double_discontinuity_generation_prevents_stale_ack_aliasing() {
        // Two discontinuities sharing one epoch (the common case: only
        // EpochTransition ever changes the epoch) must still get distinct
        // recovery identities, so an ack keyed by the first can never close
        // the second's recovery.
        let q = FrameQueue::new(8);
        let gen1 = q
            .push_discontinuity(7, DiscontinuityReason::ReorderGap)
            .expect("queue open");
        let gen2 = q
            .push_discontinuity(7, DiscontinuityReason::FrameCrc)
            .expect("queue open");
        assert_ne!(gen1, gen2);
        assert_eq!(q.recovery(), Some((gen2, 7)), "newest recovery is open");

        assert!(
            !q.acknowledge_recovery(gen1, 7),
            "stale generation must not close a later same-epoch recovery"
        );
        assert_eq!(
            q.recovery(),
            Some((gen2, 7)),
            "recovery must still be open under the current generation"
        );
        assert!(
            q.acknowledge_recovery(gen2, 7),
            "matching generation closes recovery"
        );
        assert_eq!(q.recovery(), None);
    }

    #[test]
    fn discontinuity_recovery_is_visible_before_a_blocked_consumer_observes_the_reset() {
        // Prove the fix, not just pin it: a consumer thread blocked in
        // wait_event before the push happens must see recovery already
        // open the instant it wakes with the reset event — recovery opens
        // under the same lock that makes the event poppable, so there is no
        // window where the popped reset is visible but recovery is not.
        let q = Arc::new(FrameQueue::new(8));
        let q2 = Arc::clone(&q);
        let waiter = std::thread::spawn(move || {
            let ev = q2.wait_event(Duration::from_secs(5));
            let recovery_seen_immediately = q2.recovery();
            (ev, recovery_seen_immediately)
        });
        // Give the waiter a moment to actually block before pushing.
        std::thread::sleep(Duration::from_millis(20));
        let generation = q
            .push_discontinuity(3, DiscontinuityReason::MetadataMismatch)
            .expect("queue open");

        let (ev, recovery_seen_immediately) = waiter.join().expect("waiter must not panic");
        assert_eq!(
            ev,
            Some(VideoEvent::Discontinuity {
                generation,
                epoch: 3,
                reason: DiscontinuityReason::MetadataMismatch,
            })
        );
        assert_eq!(
            recovery_seen_immediately,
            Some((generation, 3)),
            "recovery must already be open by the time the consumer observes the reset"
        );
    }

    #[test]
    fn frame_epoch_survives_the_queue() {
        let q = FrameQueue::new(8);
        assert!(q.push_frame(unit_epoch(3, 42)));
        let u = q.wait_pop(Duration::from_millis(10)).expect("frame");
        assert_eq!(u.epoch, 42);
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

        assert!(!q.push_frame(unit(0)), "push after close rejected");
    }

    #[test]
    fn close_keeps_queued_frames_poppable() {
        let q = FrameQueue::new(8);
        assert!(q.push_frame(unit(7)));
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
                    assert!(q.push_frame(unit(i)));
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
