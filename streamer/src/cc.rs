// streamer/src/cc.rs
//
// Pudica-style frame-delay congestion controller.
//
// Pure logic — no clocks, no I/O, no async, no new Cargo dependencies,
// no unsafe. The caller supplies timestamps; all time units are microseconds (u64).
//
// Composition with abr.rs:
//   effective_kbps = effective_target_kbps(abr_target_kbps, cc.target_kbps())
//   (0 = "no signal" on either side; min of the nonzero values otherwise)
//
// Transport wiring: the controller lives in the video sample-sender loop
// (transport/webrtc/sender.rs), which measures per-frame send timing and
// publishes the target into [`CcShared`]. The RTCP reader posts RR loss into
// the mailbox; the runtime bitrate task (main.rs) composes CC with ABR.

/// Per-instance configuration for [`CcController`].
///
/// Create with [`CcConfig::from_ceiling`] for sane defaults, or fill fields
/// manually and pass through [`CcConfig::sanitised`].
#[derive(Debug, Clone, Copy)]
pub struct CcConfig {
    /// Absolute bitrate floor (kbps). Target never drops below this.
    /// Sanitised to `1..=max_kbps`.
    pub min_kbps: u32,

    /// Absolute bitrate ceiling (kbps). Target never exceeds this.
    /// Sanitised to `>= 1`. This is the CC's own max; callers may also
    /// pass the ABR ceiling for the composition `min()`.
    pub max_kbps: u32,

    /// Number of frames kept in the min-filter sliding window for baseline
    /// computation. Default 60 (~500 ms at 120 fps). Sanitised to 1..=10_000.
    pub window_frames: usize,

    /// EMA denominator N (weight = 1/N). Default 8 → half-life ≈ 5.5 frames.
    pub ema_alpha_inv: u32,

    /// `smoothed / baseline > inflate_ratio` → inflate detected.
    /// Default 1.25 (25% above baseline).
    pub inflate_ratio: f64,

    /// `smoothed / baseline < deflate_ratio` → under-budget confirmed.
    /// Default 0.90 (10% below baseline).
    pub deflate_ratio: f64,

    /// Consecutive inflate-detected frames required before a decrease fires.
    /// Default 8 (~67 ms at 120 fps).
    pub inflate_frames: u32,

    /// Consecutive deflate-detected frames required before a probe-up fires.
    /// Default 16 (~133 ms at 120 fps).
    pub deflate_frames: u32,

    /// Multiplicative-decrease factor. `target *= md_factor`. Default 0.85.
    /// Clamped to (0.0, 1.0] by sanitisation.
    pub md_factor: f64,

    /// Additive probe-up step (kbps). Default `max(max_kbps / 40, 100)`.
    pub probe_additive_kbps: u32,

    /// Frames to lock out further decreases after one fires. Default 30
    /// (~250 ms at 120 fps). Prevents rapid oscillation.
    pub cooldown_frames: u32,

    /// Loss fraction at or above which `on_loss_report` fires an immediate MD.
    /// Matches `HIGH_LOSS_FRACTION` in abr.rs (0.10) for consistency.
    pub loss_threshold: f64,

    /// Absolute budget factor: triggers inflate when `smoothed_us > budget_factor *
    /// target_frame_interval_us`. Only active when `target_frame_interval_us > 0`.
    /// Default 1.0. Sanitised to finite and > 0.0 (else 1.0).
    pub budget_factor: f64,
}

impl CcConfig {
    /// Derive sane defaults from just the stream ceiling.
    pub fn from_ceiling(max_kbps: u32) -> Self {
        let max_kbps = max_kbps.max(1);
        Self {
            min_kbps: (max_kbps / 10).max(500).min(max_kbps),
            max_kbps,
            window_frames: 60,
            ema_alpha_inv: 8,
            inflate_ratio: 1.25,
            deflate_ratio: 0.90,
            inflate_frames: 8,
            deflate_frames: 16,
            md_factor: 0.85,
            probe_additive_kbps: (max_kbps / 40).max(100),
            cooldown_frames: 30,
            loss_threshold: 0.10,
            budget_factor: 1.0,
        }
    }

    /// Clamp all fields into their valid ranges. Returns a sanitised copy.
    pub fn sanitised(self) -> Self {
        let max_kbps = self.max_kbps.max(1);
        let min_kbps = self.min_kbps.clamp(1, max_kbps);
        let md_factor = if self.md_factor.is_finite() && self.md_factor > 0.0 {
            self.md_factor.min(1.0)
        } else {
            0.85
        };
        let inflate_ratio = if self.inflate_ratio.is_finite() && self.inflate_ratio > 1.0 {
            self.inflate_ratio
        } else {
            1.25
        };
        let deflate_ratio = if self.deflate_ratio.is_finite()
            && self.deflate_ratio > 0.0
            && self.deflate_ratio < 1.0
        {
            self.deflate_ratio
        } else {
            0.90
        };
        let loss_threshold = if self.loss_threshold.is_finite()
            && self.loss_threshold > 0.0
            && self.loss_threshold <= 1.0
        {
            self.loss_threshold
        } else {
            0.10
        };
        let budget_factor = if self.budget_factor.is_finite() && self.budget_factor > 0.0 {
            // 상한 1e6: budget_factor * u64::MAX 곱이 f64::INFINITY로 오버플로되면
            // 절대 트리거가 묵음 비활성화됨 (finding #4). 1e6 × u64::MAX ≈ 1.7e25 — 유한.
            self.budget_factor.min(1e6)
        } else {
            1.0
        };
        // A1: window_frames sanitised to 1..=10_000
        let window_frames = self.window_frames.clamp(1, 10_000);
        Self {
            min_kbps,
            max_kbps,
            window_frames,
            ema_alpha_inv: self.ema_alpha_inv.max(1),
            inflate_ratio,
            deflate_ratio,
            inflate_frames: self.inflate_frames.max(1),
            deflate_frames: self.deflate_frames.max(1),
            md_factor,
            probe_additive_kbps: self.probe_additive_kbps.max(1),
            // finding #2: cooldown_frames=0 → 오실레이션 방어 무력화;
            // cooldown_frames=u32::MAX → 사실상 영구 잠금 + saturating_add 방지.
            // 다른 duration 필드와 일관되게 clamp(1, 10_000) 적용.
            cooldown_frames: self.cooldown_frames.clamp(1, 10_000),
            loss_threshold,
            budget_factor,
        }
    }
}

/// Congestion verdict returned alongside the target by `on_frame`.
/// Primarily for logging/telemetry — callers use `target_kbps()` for control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcVerdict {
    /// Frame skipped without updating state (backwards or duplicate timestamp).
    Skipped,
    /// Warm-up: no baseline yet; holding at `max_kbps`.
    WarmingUp,
    /// Controller is in the dead band (no consistent signal in either direction).
    Hold,
    /// Sustained inflation detected; applied multiplicative decrease.
    Decrease,
    /// Sustained under-budget; applied additive probe-up.
    Increase,
    /// In post-decrease cooldown; inflation detected but locked out.
    CooldownHold,
}

/// Pudica-style frame-delay congestion controller.
///
/// Feed per-frame timing with [`on_frame`]; read the controlled target with
/// [`target_kbps`]. Compose with [`abr.rs`] by taking
/// `min(abr_target_kbps, cc.target_kbps())` in the apply task.
#[derive(Debug, Clone)]
pub struct CcController {
    config: CcConfig,

    /// CC-side bitrate target (kbps). Always in `[min_kbps, max_kbps]`.
    cc_target_kbps: u32,

    /// Min-filter sliding window of frame service times (μs), oldest first.
    window: std::collections::VecDeque<u64>,

    /// Current min over `window` (the "baseline"). `None` until the window
    /// first fills (`window_frames` frames processed).
    baseline_us: Option<u64>,

    /// EMA-smoothed service time (μs). `None` until the first frame.
    smoothed_us: Option<u64>,

    /// Consecutive frames where inflate condition triggered. Resets on any
    /// non-inflate frame or after a decrease fires.
    inflate_count: u32,

    /// Consecutive frames where score < `deflate_ratio`. Resets on any
    /// non-deflate frame or after a probe-up fires.
    deflate_count: u32,

    /// Frames remaining in the post-decrease lockout window.
    cooldown_remaining: u32,

    /// Monotonic timestamp of the last processed frame (μs), for
    /// backwards-timestamp detection.
    last_send_done_us: Option<u64>,
}

impl CcController {
    /// Start optimistic at the ceiling. The controller will adapt downward
    /// as frame-delay evidence arrives.
    pub fn new(config: CcConfig) -> Self {
        let config = config.sanitised();
        let max_kbps = config.max_kbps;
        // A3: VecDeque::with_capacity(config.window_frames)
        Self {
            window: std::collections::VecDeque::with_capacity(config.window_frames),
            config,
            cc_target_kbps: max_kbps,
            baseline_us: None,
            smoothed_us: None,
            inflate_count: 0,
            deflate_count: 0,
            cooldown_remaining: 0,
            last_send_done_us: None,
        }
    }

    /// Current CC bitrate target (kbps). Use `min(abr_target, cc.target_kbps())`
    /// as the effective encoder bitrate.
    // Runtime wiring consumes the (target, verdict) returned by `on_frame`
    // and publishes via CcShared; this read-only accessor remains for tests
    // and future callers that need the target between frames.
    #[allow(dead_code)]
    pub fn target_kbps(&self) -> u32 {
        self.cc_target_kbps
    }

    /// Feed one video frame's send timing.
    ///
    /// - `send_start_us`: monotonic timestamp when the first RTP packet of
    ///   this frame began being written to the socket (μs).
    /// - `send_done_us`: monotonic timestamp when the last RTP packet of
    ///   this frame finished writing (μs).
    /// - `frame_size_bytes`: encoded frame payload size (bytes). Accepted for
    ///   completeness and future link-capacity estimation; not used by the
    ///   current control law.
    /// - `target_frame_interval_us`: reciprocal of the target frame rate
    ///   (e.g. 8333 μs for 120 fps). When > 0, enables the absolute budget
    ///   trigger alongside the relative score trigger.
    ///
    /// Returns `(cc_target_kbps, verdict)`. The target is always in
    /// `[min_kbps, max_kbps]`. On [`CcVerdict::Skipped`] the target is unchanged.
    pub fn on_frame(
        &mut self,
        send_start_us: u64,
        send_done_us: u64,
        _frame_size_bytes: u64,
        target_frame_interval_us: u64,
    ) -> (u32, CcVerdict) {
        // -- Timestamp validation -------------------------------------------------
        let service_us = send_done_us.saturating_sub(send_start_us);

        // Backwards or duplicate send_done_us → Skipped (A4: <= last)
        if self.last_send_done_us.is_some_and(|last| send_done_us <= last) {
            return (self.cc_target_kbps, CcVerdict::Skipped);
        }
        self.last_send_done_us = Some(send_done_us);

        // -- EMA update (saturating arithmetic) -----------------------------------
        let n = self.config.ema_alpha_inv as u64;
        self.smoothed_us = Some(match self.smoothed_us {
            None => service_us,
            Some(s) => s.saturating_mul(n - 1).saturating_add(service_us) / n,
        });

        // -- Window & baseline update ---------------------------------------------
        self.window.push_back(service_us);
        if self.window.len() > self.config.window_frames {
            self.window.pop_front();
        }
        if self.window.len() == self.config.window_frames {
            self.baseline_us = Some(self.window.iter().copied().min().unwrap_or(1));
        }

        // -- No baseline yet → warmup ---------------------------------------------
        if self.baseline_us.is_none() {
            self.cooldown_remaining = self.cooldown_remaining.saturating_sub(1);
            return (self.cc_target_kbps, CcVerdict::WarmingUp);
        }

        let baseline = self.baseline_us.unwrap_or(1).max(1);
        let smoothed = self.smoothed_us.unwrap_or(service_us);

        // -- Congestion score (relative) ------------------------------------------
        let score = smoothed as f64 / baseline as f64;

        // -- A1: Absolute budget trigger ------------------------------------------
        // Only active when target_frame_interval_us > 0; disabled for interval==0.
        let absolute_inflate = target_frame_interval_us > 0
            && (smoothed as f64) > self.config.budget_factor * (target_frame_interval_us as f64);

        // -- Counter update -------------------------------------------------------
        let relative_inflate = score > self.config.inflate_ratio;
        let inflate_triggered = relative_inflate || absolute_inflate;

        if inflate_triggered {
            // finding #3: saturating_add — debug 빌드 overflow panic 방지.
            self.inflate_count = self.inflate_count.saturating_add(1);
            self.deflate_count = 0;
        } else if score < self.config.deflate_ratio {
            self.deflate_count = self.deflate_count.saturating_add(1);
            self.inflate_count = 0;
        } else {
            self.inflate_count = 0;
            self.deflate_count = 0;
        }

        // -- Control action -------------------------------------------------------
        let verdict;

        if self.inflate_count >= self.config.inflate_frames {
            if self.cooldown_remaining > 0 {
                verdict = CcVerdict::CooldownHold;
            } else {
                // Multiplicative decrease
                let new_target = (self.cc_target_kbps as f64 * self.config.md_factor) as u32;
                self.cc_target_kbps = new_target.clamp(self.config.min_kbps, self.config.max_kbps);
                // +1: on_frame end에서 즉시 saturating_sub(1)되므로,
                // 실효 쿨다운 기간이 정확히 cooldown_frames 프레임이 되도록 보정.
                // 이로써 MD→MD 최소 주기 = cooldown_frames + inflate_frames (§6).
                self.cooldown_remaining = self.config.cooldown_frames.saturating_add(1);
                self.inflate_count = 0;
                verdict = CcVerdict::Decrease;
            }
        } else if self.deflate_count >= self.config.deflate_frames {
            // Additive probe-up
            self.cc_target_kbps = self
                .cc_target_kbps
                .saturating_add(self.config.probe_additive_kbps)
                .min(self.config.max_kbps);
            self.deflate_count = 0;
            verdict = CcVerdict::Increase;
        } else {
            verdict = CcVerdict::Hold;
        }

        // finding #1: 쿨다운이 만료되는 순간(remaining 1→0) inflate_count를 리셋.
        // 이로써 쿨다운 만료 후 inflate_frames 프레임이 새로 쌓여야 다음 Decrease 발동:
        //   최소 MD→MD 주기 = cooldown_frames + inflate_frames (스펙 §6 보장).
        // 대안(쿨다운 중 inflate_count 미증가)은 CooldownHold verdict 반환을 방해하여
        // T06 등의 기존 계약을 깨므로 채택하지 않음.
        let was_cooling = self.cooldown_remaining > 0;
        self.cooldown_remaining = self.cooldown_remaining.saturating_sub(1);
        if was_cooling && self.cooldown_remaining == 0 {
            self.inflate_count = 0;
        }
        (self.cc_target_kbps, verdict)
    }

    /// Feed a packet-loss observation (fraction in `[0.0, 1.0]`).
    ///
    /// Loss ≥ `config.loss_threshold` triggers an immediate multiplicative
    /// decrease, bypassing `inflate_frames` and cooldown (matches the
    /// "react down hard on congestion" philosophy of abr.rs). Loss below the
    /// threshold is a no-op. Non-finite input is sanitised to 0 (no-op).
    ///
    /// Returns the new target (kbps).
    pub fn on_loss_report(&mut self, loss_fraction: f64) -> u32 {
        let loss = if loss_fraction.is_finite() {
            loss_fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if loss >= self.config.loss_threshold {
            let new_target = (self.cc_target_kbps as f64 * self.config.md_factor) as u32;
            self.cc_target_kbps = new_target.clamp(self.config.min_kbps, self.config.max_kbps);
            // Reset cooldown (overwrite even if already in cooldown).
            // on_loss_report는 on_frame 외부에서 호출되므로 end -1 보정 없이 그대로.
            self.cooldown_remaining = self.config.cooldown_frames;
            self.inflate_count = 0;
            // Loss contradicts accumulated under-budget evidence: without this
            // reset, deflate_count at deflate_frames-1 lets the very next
            // under-budget frame probe the target straight back up, reversing
            // this decrease in the same sender-loop iteration (W12).
            self.deflate_count = 0;
        }
        self.cc_target_kbps
    }
}

// ── Transport wiring state ────────────────────────────────────────────────
//
// Everything above is pure control law. The items below are the shared state
// bridging the three tasks that jointly drive congestion control at runtime:
//
//   - the video sample-sender loop (hot path) feeds `on_frame` and publishes
//     the resulting target via [`CcShared::publish_target_for`];
//   - the RTCP reader posts Receiver Report loss fractions into the mailbox
//     ([`CcShared::post_loss_for`]), drained by the sender loop before each
//     frame ([`CcShared::take_loss_for`]);
//   - the runtime bitrate task reads the published target and composes it
//     with the ABR target via [`effective_target_kbps`].
//
// Every write is tagged with the generation obtained from
// [`CcShared::begin_generation`] at stream setup, so a superseded writer
// (stream re-setup leaves the old sender/RTCP tasks running) reads as
// inactive instead of leaking into the new stream — see the CcShared docs.
//
// Atomics only — still no clocks, no I/O, no async in this module.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Loss-mailbox sentinel: no unread report. A valid packed post always has
/// its value half ≤ 255 (RFC 3550 `fraction_lost`), so `u64::MAX` (value half
/// `u32::MAX`) cannot collide with a real post.
const LOSS_MAILBOX_EMPTY: u64 = u64::MAX;

/// Pack a (generation, value) pair into one atomic word so readers can
/// validate the writer's generation and the value in a single load.
fn pack(generation: u32, value: u32) -> u64 {
    (u64::from(generation) << 32) | u64::from(value)
}

fn unpack(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// Cross-task CC state. One instance per transport, shared by the sender
/// loop (owner of the [`CcController`]), the RTCP reader, and the runtime
/// bitrate apply task.
///
/// ## Generations
///
/// `WebRtcVideo::setup` can run more than once per transport (stream
/// replacement), and `create_track` spawns a new sample-sender task without
/// stopping the previous one — the old task keeps draining the same queue and
/// would keep publishing its (stale, or worse, supposed-to-be-disabled) CC
/// target into this struct. Every writer therefore tags its writes with the
/// generation it was created under ([`Self::begin_generation`]), and readers
/// validate the tag: a write from a superseded generation reads as "inactive"
/// (target) or "not ours" (loss mailbox). A ghost write can at worst mask the
/// fresh target for a single frame interval (readers then see inactive → the
/// composition falls back to ABR-only, never to a stale CC value).
#[derive(Debug)]
pub struct CcShared {
    /// Current writer generation. Bumped by [`Self::begin_generation`] at
    /// each stream setup; writers created under an older value are ghosts.
    generation: AtomicU32,
    /// `pack(generation, kbps)` of the latest published CC target.
    /// Reads as 0 ("inactive") unless the packed generation is current.
    target_packed: AtomicU64,
    /// `pack(generation, fraction_lost)` of the latest unread RR loss report,
    /// or [`LOSS_MAILBOX_EMPTY`]. A newer post overwrites an unread older one
    /// (latest-wins); the draining reader ignores non-current generations and
    /// leaves posts it does not own in place.
    loss_packed: AtomicU64,
    /// Target frame interval (μs) for the absolute budget trigger.
    /// 0 = unknown frame rate (trigger disabled). Set at stream setup.
    frame_interval_us: AtomicU64,
}

impl CcShared {
    pub fn new() -> Self {
        Self {
            generation: AtomicU32::new(0),
            target_packed: AtomicU64::new(pack(0, 0)),
            loss_packed: AtomicU64::new(LOSS_MAILBOX_EMPTY),
            frame_interval_us: AtomicU64::new(0),
        }
    }

    /// Start a new writer generation: invalidates every outstanding writer
    /// (their subsequent writes read as inactive), resets the published
    /// target to "inactive", and discards any unread loss report. Called at
    /// stream setup; the returned generation must be handed to the writers
    /// created for that stream.
    pub fn begin_generation(&self) -> u32 {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.target_packed.store(pack(generation, 0), Ordering::Release);
        self.loss_packed.store(LOSS_MAILBOX_EMPTY, Ordering::Release);
        generation
    }

    /// Publish the sender loop's latest CC target. A stale `generation`
    /// (superseded by a newer [`Self::begin_generation`]) is dropped when
    /// detectable here; the read side validates again, so even a racing stale
    /// store can only read back as "inactive", never as a live stale target.
    pub fn publish_target_for(&self, generation: u32, kbps: u32) {
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        self.target_packed.store(pack(generation, kbps), Ordering::Release);
    }

    /// Latest CC target (kbps); 0 = CC inactive (no controller yet, disabled,
    /// or the last write came from a superseded generation).
    pub fn target_kbps(&self) -> u32 {
        let current = self.generation.load(Ordering::Acquire);
        let (generation, kbps) = unpack(self.target_packed.load(Ordering::Acquire));
        if generation == current { kbps } else { 0 }
    }

    /// Post an RR loss observation (`fraction_lost`, fixed-point /256).
    /// Overwrites an unread previous post: between two sender-loop drains the
    /// newest report wins, matching abr.rs's "latest observation" semantics.
    /// Posts from a superseded generation are dropped.
    pub fn post_loss_for(&self, generation: u32, fraction_lost: u8) {
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        self.loss_packed
            .store(pack(generation, u32::from(fraction_lost)), Ordering::Release);
    }

    /// Drain the loss mailbox. Returns the loss fraction in `[0.0, 1.0]`, or
    /// `None` when there is no unread report owned by `generation`. Never
    /// returns the same post twice, and never removes a post belonging to a
    /// different generation (a ghost drain cannot steal the owner's report).
    pub fn take_loss_for(&self, generation: u32) -> Option<f64> {
        loop {
            let current = self.loss_packed.load(Ordering::Acquire);
            if current == LOSS_MAILBOX_EMPTY {
                return None;
            }
            let (post_generation, fraction) = unpack(current);
            if post_generation != generation {
                return None; // not ours — leave it for its owner
            }
            match self.loss_packed.compare_exchange_weak(
                current,
                LOSS_MAILBOX_EMPTY,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(f64::from(fraction) / 256.0),
                Err(_) => continue, // a newer post landed — re-examine it
            }
        }
    }

    /// Set the target frame interval (μs). 0 disables the absolute budget
    /// trigger in [`CcController::on_frame`].
    pub fn set_frame_interval_us(&self, us: u64) {
        self.frame_interval_us.store(us, Ordering::Release);
    }

    /// Target frame interval (μs); 0 = unknown.
    pub fn frame_interval_us(&self) -> u64 {
        self.frame_interval_us.load(Ordering::Acquire)
    }
}

impl Default for CcShared {
    fn default() -> Self {
        Self::new()
    }
}

/// Compose the ABR and CC targets into the effective encoder target.
///
/// 0 means "no signal" on either side (adaptation disabled, or CC not yet
/// active): the other side's value passes through unchanged, so enabling CC
/// can never *raise* the target above what ABR alone would have requested,
/// and a disabled controller never drags the target to 0.
pub fn effective_target_kbps(abr_kbps: u32, cc_kbps: u32) -> u32 {
    match (abr_kbps, cc_kbps) {
        (0, cc) => cc,
        (abr, 0) => abr,
        (abr, cc) => abr.min(cc),
    }
}

/// Target frame interval (μs) from a frame rate (fps).
///
/// 0 fps → 0 (unknown; disables the absolute budget trigger). Rates above
/// 1_000_000 fps truncate to 0 μs, which likewise disables the trigger.
pub fn interval_us_from_fps(fps: u32) -> u64 {
    if fps == 0 { 0 } else { 1_000_000 / u64::from(fps) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default controller with EMA smoothing (ema_alpha_inv=8). Good for tests
    /// that do not require precise per-frame inflate/deflate counting.
    fn ctl(max_kbps: u32) -> CcController {
        CcController::new(CcConfig::from_ceiling(max_kbps))
    }

    /// Controller with ema_alpha_inv=1 so smoothed_us = service_us on every frame
    /// (no lag). Use for tests that count exact inflate/deflate frames, because
    /// with alpha_inv=8 the EMA ramps up over ~2 frames before crossing the
    /// inflate_ratio threshold — making "exactly N frames → verdict" assertions
    /// unreliable.
    fn ctl_fast(max_kbps: u32) -> CcController {
        CcController::new(CcConfig {
            ema_alpha_inv: 1,
            ..CcConfig::from_ceiling(max_kbps)
        })
    }

    // 120fps = 8333 μs
    const INTERVAL_120FPS: u64 = 8_333;
    // Baseline service time: 50% of interval = 4166 μs (comfortable normal state)
    const NORMAL_SERVICE: u64 = 4_166;

    /// Feed n frames with given service_us, each send_done monotonically increasing.
    fn feed_n(
        cc: &mut CcController,
        n: u32,
        service_us: u64,
        start_ts: &mut u64,
    ) -> (u32, CcVerdict) {
        let mut last = (0, CcVerdict::Hold);
        for _ in 0..n {
            let s = *start_ts;
            let e = s + service_us;
            last = cc.on_frame(s, e, 10_000, INTERVAL_120FPS);
            *start_ts = e + 1;
        }
        last
    }


    // T01
    #[test]
    fn starts_at_max_kbps() {
        let cc = ctl(20_000);
        assert_eq!(cc.target_kbps(), 20_000);
    }

    // T02
    #[test]
    fn warming_up_returns_max_until_window_fills() {
        let mut cc = ctl(20_000);
        let window = cc.config.window_frames;
        let mut ts = 0u64;

        // window_frames - 1 frames → all WarmingUp, target unchanged
        for _ in 0..(window - 1) {
            let (t, v) = cc.on_frame(ts, ts + NORMAL_SERVICE, 10_000, INTERVAL_120FPS);
            assert_eq!(v, CcVerdict::WarmingUp, "expected WarmingUp before window fills");
            assert_eq!(t, 20_000);
            ts += NORMAL_SERVICE + 1;
        }

        // window_frames-th frame → verdict is no longer WarmingUp
        let (_, v) = cc.on_frame(ts, ts + NORMAL_SERVICE, 10_000, INTERVAL_120FPS);
        assert_ne!(v, CcVerdict::WarmingUp, "expected non-WarmingUp after window fills");
    }

    // T03
    #[test]
    fn steady_state_hold() {
        let mut cc = ctl(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;

        // Fill window
        feed_n(&mut cc, window, NORMAL_SERVICE, &mut ts);

        // 60 more frames at NORMAL_SERVICE
        for _ in 0..60 {
            let (t, v) = cc.on_frame(ts, ts + NORMAL_SERVICE, 10_000, INTERVAL_120FPS);
            ts += NORMAL_SERVICE + 1;
            assert_eq!(v, CcVerdict::Hold, "expected Hold in steady state");
            assert_eq!(t, 20_000);
        }
    }

    // T04 — uses ctl_fast (ema_alpha_inv=1) so score = service_us/baseline
    // immediately, making "exactly inflate_frames frames → Decrease" deterministic.
    #[test]
    fn inflate_triggers_decrease_within_inflate_frames() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;

        // Fill window with normal frames
        feed_n(&mut cc, window, NORMAL_SERVICE, &mut ts);

        let inflate_frames = cc.config.inflate_frames;
        // 2× NORMAL_SERVICE → score = 2.0 > inflate_ratio=1.25 from frame 1
        let congested_service = NORMAL_SERVICE * 2;

        let initial_target = cc.target_kbps();
        let (_, last_verdict) = feed_n(&mut cc, inflate_frames, congested_service, &mut ts);

        assert_eq!(last_verdict, CcVerdict::Decrease);
        assert!(cc.target_kbps() < initial_target, "target should have decreased");
    }

    // T05 — ctl_fast so exactly inflate_frames congested frames trigger Decrease.
    #[test]
    fn md_factor_applied_exactly() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;

        // Fill window + 60 extra normal frames (target stays max_kbps=20000)
        feed_n(&mut cc, window + 60, NORMAL_SERVICE, &mut ts);
        assert_eq!(cc.target_kbps(), 20_000);

        let inflate_frames = cc.config.inflate_frames;
        let congested_service = NORMAL_SERVICE * 2; // score=2.0 from frame 1
        feed_n(&mut cc, inflate_frames, congested_service, &mut ts);

        // floor(20000 * 0.85) = 17000
        assert_eq!(cc.target_kbps(), 17_000);
    }

    // T06 — ctl_fast so the second batch of inflate frames cleanly counts to 8.
    #[test]
    fn cooldown_prevents_immediate_second_decrease() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;

        feed_n(&mut cc, window + 60, NORMAL_SERVICE, &mut ts);

        let inflate_frames = cc.config.inflate_frames;
        let congested_service = NORMAL_SERVICE * 2;

        // First decrease
        feed_n(&mut cc, inflate_frames, congested_service, &mut ts);
        assert_eq!(cc.target_kbps(), 17_000);

        // Immediately apply more inflate frames — cooldown still > 0
        let target_before = cc.target_kbps();
        let (_, last_verdict) = feed_n(&mut cc, inflate_frames, congested_service, &mut ts);
        assert_eq!(last_verdict, CcVerdict::CooldownHold);
        assert_eq!(cc.target_kbps(), target_before, "target must not change during cooldown");
    }

    // T07 — ctl_fast for exact frame counting.
    #[test]
    fn cooldown_allows_decrease_after_expiry() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;

        feed_n(&mut cc, window + 60, NORMAL_SERVICE, &mut ts);

        let inflate_frames = cc.config.inflate_frames;
        let cooldown_frames = cc.config.cooldown_frames;
        let congested_service = NORMAL_SERVICE * 2;

        // First decrease
        feed_n(&mut cc, inflate_frames, congested_service, &mut ts);
        let after_first = cc.target_kbps();

        // Exhaust cooldown with normal frames (score=1.0 → dead band, inflate_count=0)
        feed_n(&mut cc, cooldown_frames, NORMAL_SERVICE, &mut ts);

        // Second round of inflation → second Decrease
        let (_, v) = feed_n(&mut cc, inflate_frames, congested_service, &mut ts);
        assert_eq!(v, CcVerdict::Decrease);
        assert!(cc.target_kbps() < after_first, "second decrease should lower target further");
    }

    // T08 — deflate_triggers_probe_up
    //
    // With a min-filter baseline, adding any value v < baseline immediately makes
    // v the new minimum, so score = v/v = 1.0 (dead band). The only reliable
    // deflate trigger is service_us = 0: baseline clamps to 1 (unwrap_or(1)),
    // EMA = 0, score = 0/1 = 0 < deflate_ratio=0.90.
    //
    // Precondition: a prior Decrease gets target below max_kbps so the probe-up
    // produces a measurable increase (not silently clamped to max_kbps).
    #[test]
    fn deflate_triggers_probe_up() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;
        let inflate_frames = cc.config.inflate_frames;
        let deflate_frames = cc.config.deflate_frames;
        let probe_step = cc.config.probe_additive_kbps;

        // Warmup + Decrease to get target below max_kbps.
        feed_n(&mut cc, window + 60, NORMAL_SERVICE, &mut ts);
        feed_n(&mut cc, inflate_frames, NORMAL_SERVICE * 2, &mut ts); // → 17000
        let before_probe = cc.target_kbps(); // 17000

        // service_us=0 → min(window)=0 → baseline=1, EMA=0, score=0 → deflate.
        // Cooldown is still active but only blocks Decrease, not Increase.
        let (t, v) = feed_n(&mut cc, deflate_frames, 0, &mut ts);
        assert_eq!(v, CcVerdict::Increase);
        assert_eq!(t, before_probe + probe_step, "target must increase by exactly probe_step");
    }

    // T09 — probe_bounded_by_max_kbps
    //
    // Uses service_us=0 to trigger deflate (same reason as T08). An oversized
    // probe_additive_kbps ensures the naive sum would overshoot max_kbps;
    // verifies the `.min(max_kbps)` clamp in the Increase path.
    // Target starts at max_kbps (no prior decrease needed — if probe fires at
    // max_kbps, min(max+huge, max) = max, confirming the clamp).
    #[test]
    fn probe_bounded_by_max_kbps() {
        let max_kbps = 20_000u32;
        let cfg = CcConfig {
            min_kbps: 1_000,
            max_kbps,
            window_frames: 60,
            ema_alpha_inv: 1,
            inflate_ratio: 1.25,
            deflate_ratio: 0.90,
            inflate_frames: 8,
            deflate_frames: 16,
            md_factor: 0.85,
            probe_additive_kbps: max_kbps, // massive → must clamp to max_kbps
            cooldown_frames: 30,
            loss_threshold: 0.10,
            budget_factor: 1.0,
        };
        let mut cc = CcController::new(cfg);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;

        feed_n(&mut cc, window, NORMAL_SERVICE, &mut ts);

        let deflate_frames = cc.config.deflate_frames;
        // service_us=0 → EMA=0, baseline=1, score=0 → deflate fires reliably.
        let (t, v) = feed_n(&mut cc, deflate_frames, 0, &mut ts);
        assert_eq!(v, CcVerdict::Increase);
        assert_eq!(t, max_kbps, "probe must not exceed max_kbps");
    }

    // T10
    #[test]
    fn decrease_bounded_by_min_kbps() {
        let cfg = CcConfig {
            min_kbps: 5_000,
            max_kbps: 6_000,
            window_frames: 60,
            ema_alpha_inv: 8,
            inflate_ratio: 1.25,
            deflate_ratio: 0.90,
            inflate_frames: 8,
            deflate_frames: 16,
            md_factor: 0.85,
            probe_additive_kbps: 100,
            cooldown_frames: 30,
            loss_threshold: 0.10,
            budget_factor: 1.0,
        };
        let mut cc = CcController::new(cfg);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;

        feed_n(&mut cc, window, NORMAL_SERVICE, &mut ts);

        // Drive down to near min_kbps with repeated decreases
        for _ in 0..20 {
            let inflate_frames = cc.config.inflate_frames;
            let cooldown_frames = cc.config.cooldown_frames;
            let congested_service = NORMAL_SERVICE * 2;
            feed_n(&mut cc, inflate_frames, congested_service, &mut ts);
            feed_n(&mut cc, cooldown_frames, NORMAL_SERVICE, &mut ts);
        }

        assert!(
            cc.target_kbps() >= 5_000,
            "target {} must not go below min_kbps 5000",
            cc.target_kbps()
        );
        assert!(cc.target_kbps() > 0, "target must not be zero");
    }

    // T11
    #[test]
    fn jitter_noise_no_reaction() {
        let mut cc = ctl(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;

        feed_n(&mut cc, window, NORMAL_SERVICE, &mut ts);

        // Alternating 0.85x and 1.15x of NORMAL_SERVICE
        // score ~0.85 and ~1.15 — neither exceeds inflate(1.25) nor hits
        // deflate(0.90) consistently enough to accumulate
        let target_before = cc.target_kbps();
        for i in 0..200u32 {
            let svc = if i % 2 == 0 {
                (NORMAL_SERVICE as f64 * 0.85) as u64
            } else {
                (NORMAL_SERVICE as f64 * 1.15) as u64
            };
            let (_, v) = cc.on_frame(ts, ts + svc, 10_000, INTERVAL_120FPS);
            ts += svc + 1;
            // Not Decrease or Increase
            assert!(
                v == CcVerdict::Hold || v == CcVerdict::WarmingUp || v == CcVerdict::CooldownHold,
                "unexpected verdict {v:?} at frame {i}"
            );
        }
        assert_eq!(cc.target_kbps(), target_before, "target must not change on jitter");
    }

    // T12
    #[test]
    fn backwards_timestamp_is_skipped() {
        let mut cc = ctl(20_000);
        let ts = 100u64;

        // Normal frame
        cc.on_frame(ts, ts + NORMAL_SERVICE, 10_000, INTERVAL_120FPS);
        let target_after_first = cc.target_kbps();

        // Backwards: send_done_us < previous send_done_us
        let (t, v) = cc.on_frame(ts + NORMAL_SERVICE + 1, ts - 1, 10_000, INTERVAL_120FPS);
        assert_eq!(v, CcVerdict::Skipped);
        assert_eq!(t, target_after_first, "target must not change on Skipped");
    }

    // T13
    #[test]
    fn duplicate_timestamp_is_skipped() {
        let mut cc = ctl(20_000);
        let ts = 100u64;

        cc.on_frame(ts, ts + NORMAL_SERVICE, 10_000, INTERVAL_120FPS);
        let target_after_first = cc.target_kbps();

        // Duplicate: same send_done_us
        let (t, v) = cc.on_frame(ts + NORMAL_SERVICE + 1, ts + NORMAL_SERVICE, 10_000, INTERVAL_120FPS);
        assert_eq!(v, CcVerdict::Skipped, "duplicate send_done_us must be Skipped");
        assert_eq!(t, target_after_first);
    }

    // T14
    #[test]
    fn zero_frame_size_no_panic() {
        let mut cc = ctl(20_000);
        let ts = 100u64;
        // Must not panic; frame_size_bytes = 0
        let (_, v) = cc.on_frame(ts, ts + NORMAL_SERVICE, 0, INTERVAL_120FPS);
        assert_ne!(v, CcVerdict::Skipped);
    }

    // T15
    #[test]
    fn zero_target_interval_no_panic() {
        let mut cc = ctl(20_000);
        let ts = 100u64;
        // target_frame_interval_us = 0 → absolute trigger disabled, no panic
        let _ = cc.on_frame(ts, ts + NORMAL_SERVICE, 10_000, 0);
    }

    // T16
    #[test]
    fn min_gt_max_sanitised_no_panic() {
        let cfg = CcConfig {
            min_kbps: 50_000,
            max_kbps: 1_000,
            window_frames: 60,
            ema_alpha_inv: 8,
            inflate_ratio: 1.25,
            deflate_ratio: 0.90,
            inflate_frames: 8,
            deflate_frames: 16,
            md_factor: 0.85,
            probe_additive_kbps: 100,
            cooldown_frames: 30,
            loss_threshold: 0.10,
            budget_factor: 1.0,
        };
        let cc = CcController::new(cfg);
        // After sanitisation min_kbps = max_kbps = 1000, target = 1000
        assert_eq!(cc.target_kbps(), 1_000);
    }

    // T17 — ctl_fast so the Decrease lands at exactly 17000.
    #[test]
    fn composition_min_behavior() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;
        let inflate_frames = cc.config.inflate_frames;

        feed_n(&mut cc, window + 60, NORMAL_SERVICE, &mut ts);
        feed_n(&mut cc, inflate_frames, NORMAL_SERVICE * 2, &mut ts);
        // cc_target = floor(20000 * 0.85) = 17000
        let cc_t = cc.target_kbps();
        assert_eq!(cc_t, 17_000);

        // CC lower → CC dominates
        let abr_t: u32 = 20_000;
        assert_eq!(cc_t.min(abr_t), 17_000);

        // ABR lower → ABR dominates
        let cc_t2: u32 = 25_000;
        let abr_t2: u32 = 20_000;
        assert_eq!(cc_t2.min(abr_t2), 20_000);
    }

    // T18
    #[test]
    fn on_loss_report_below_threshold_is_noop() {
        let mut cc = ctl(20_000);
        let t = cc.on_loss_report(0.05);
        assert_eq!(t, 20_000, "loss below threshold must not change target");
    }

    // T19
    #[test]
    fn on_loss_report_high_loss_fires_md() {
        let mut cc = ctl(20_000);
        let t = cc.on_loss_report(0.20);
        // floor(20000 * 0.85) = 17000
        assert_eq!(t, 17_000);
    }

    // T20 — ctl_fast for exact per-frame control; traces the full
    // drop → cooldown → recovery sequence.
    #[test]
    fn step_bandwidth_drop_recovery_sequence() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let min_kbps = cc.config.min_kbps;
        let max_kbps = cc.config.max_kbps;
        let window = cc.config.window_frames as u32;
        let inflate_frames = cc.config.inflate_frames;
        let cooldown_frames = cc.config.cooldown_frames;
        let deflate_frames = cc.config.deflate_frames;

        let check_bounds = |t: u32| {
            assert!(
                t >= min_kbps && t <= max_kbps,
                "target {t} out of [{min_kbps}, {max_kbps}]"
            );
        };

        // Warmup + stable
        let (t, _) = feed_n(&mut cc, window + 60, NORMAL_SERVICE, &mut ts);
        check_bounds(t);

        // Inflate → Decrease
        let (t, v) = feed_n(&mut cc, inflate_frames, NORMAL_SERVICE * 2, &mut ts);
        check_bounds(t);
        assert_eq!(v, CcVerdict::Decrease);

        // Cooldown
        let (t, _) = feed_n(&mut cc, cooldown_frames, NORMAL_SERVICE, &mut ts);
        check_bounds(t);

        // Deflate → Increase (service_us=0 triggers deflate; see T08 comment)
        let (t, v) = feed_n(&mut cc, deflate_frames, 0, &mut ts);
        check_bounds(t);
        assert_eq!(v, CcVerdict::Increase, "expected Increase after deflate phase");
    }

    // T21 — baseline_pollution_absolute_trigger_still_decreases
    //
    // Once the window fills entirely with congested samples the relative score
    // collapses to 1.0 (min-filter baseline = current value). The absolute trigger
    // (smoothed > budget_factor * interval) must continue to fire Decreases.
    // ema_alpha_inv=1 makes the transition deterministic: score = service/baseline.
    #[test]
    fn baseline_pollution_absolute_trigger_still_decreases() {
        let cfg = CcConfig {
            min_kbps: 1_000,
            max_kbps: 20_000,
            window_frames: 10,
            ema_alpha_inv: 1, // smoothed = service_us every frame
            inflate_ratio: 1.25,
            deflate_ratio: 0.90,
            inflate_frames: 8,
            deflate_frames: 16,
            md_factor: 0.85,
            probe_additive_kbps: 100,
            cooldown_frames: 10,
            loss_threshold: 0.10,
            budget_factor: 1.0,
        };
        let mut cc = CcController::new(cfg);
        let mut ts = 0u64;

        // 3× target interval → absolute trigger (smoothed > 1.0 × interval) fires.
        let congested_service = INTERVAL_120FPS * 3;
        let inflate_frames = cc.config.inflate_frames;
        let cooldown_frames = cc.config.cooldown_frames;

        // Phase 1: window 채우기 + 첫 Decrease 획득 (상대 트리거 활성 중).
        // feed_n은 마지막 프레임 결과만 반환하므로 target 감소로 Decrease 감지.
        let initial_target = cc.target_kbps();
        // window_frames(10) 워밍업 + inflate_frames(8) + cooldown_frames(10) + 여유
        let phase1_budget = (cc.config.window_frames as u32)
            .saturating_add(inflate_frames * 2)
            .saturating_add(cooldown_frames)
            .saturating_add(5);
        feed_n(&mut cc, phase1_budget, congested_service, &mut ts);
        // finding #5: phase1 Decrease 전제를 명시적으로 검증.
        let phase1_decrease = cc.target_kbps() < initial_target;
        // 혹시 쿨다운 중이면 소모
        feed_n(&mut cc, cooldown_frames + 1, congested_service, &mut ts);
        assert!(phase1_decrease, "phase1 must get at least one Decrease (relative trigger active)");

        // Phase 2: window now full of congested samples → baseline = congested_service.
        // Relative score = smoothed/baseline = 1.0 → dead band. Only absolute trigger fires.
        // finding #5 재구성: 개별 프레임 루프로 Decrease를 직접 포착
        // (feed_n 마지막 프레임 반환 문제 회피).
        let target_before_phase2 = cc.target_kbps();
        let mut found_abs_decrease = false;
        let phase2_budget = (cooldown_frames + inflate_frames) * 3;
        for _ in 0..phase2_budget {
            let s = ts;
            let e = s + congested_service;
            let (_, v) = cc.on_frame(s, e, 10_000, INTERVAL_120FPS);
            ts = e + 1;
            if v == CcVerdict::Decrease {
                found_abs_decrease = true;
                break;
            }
        }

        assert!(
            found_abs_decrease,
            "absolute trigger must keep firing Decreases once window is fully congested; \
             target_before_phase2={target_before_phase2}, target_now={}",
            cc.target_kbps()
        );
        assert!(cc.target_kbps() >= cc.config.min_kbps, "target must not fall below min_kbps");
    }

    // T22 — zero_interval_disables_absolute_trigger
    // With target_frame_interval_us = 0 and service times far above any interval
    // but flat relative to baseline, assert NO decrease ever fires.
    #[test]
    fn zero_interval_disables_absolute_trigger() {
        // Use a config where relative score stays in dead band (1.0 = no inflate/deflate)
        // but service times far exceed any reasonable interval.
        // We feed constant service times so relative score = 1.0 (dead band).
        // With interval=0, absolute trigger is disabled → no decrease.
        let cfg = CcConfig {
            min_kbps: 1_000,
            max_kbps: 20_000,
            window_frames: 10,
            ema_alpha_inv: 1, // alpha=1 → smoothed = last service directly
            inflate_ratio: 1.25,
            deflate_ratio: 0.90,
            inflate_frames: 8,
            deflate_frames: 16,
            md_factor: 0.85,
            probe_additive_kbps: 100,
            cooldown_frames: 30,
            loss_threshold: 0.10,
            budget_factor: 1.0,
        };
        let mut cc = CcController::new(cfg);
        let mut ts = 0u64;
        let _window = cc.config.window_frames as u32;

        // Constant high service time → once window fills, baseline = smoothed = same
        // value → score = 1.0 → dead band → no Decrease, no Increase
        let constant_service = INTERVAL_120FPS * 100; // massively above any interval

        // Feed enough to fill the window
        for _ in 0.._window {
            cc.on_frame(ts, ts + constant_service, 10_000, 0); // interval=0
            ts += constant_service + 1;
        }

        // Feed 200 more frames; verify no Decrease occurs
        for _ in 0..200 {
            let (_, v) = cc.on_frame(ts, ts + constant_service, 10_000, 0);
            ts += constant_service + 1;
            assert_ne!(
                v,
                CcVerdict::Decrease,
                "absolute trigger must be disabled when target_frame_interval_us=0"
            );
        }
    }

    // T24 — pin: 지속 혼잡에서 연속 두 Decrease 간격 >= cooldown_frames + inflate_frames (§6)
    #[test]
    fn pin_cooldown_minimum_md_to_md_interval() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;
        let inflate_frames = cc.config.inflate_frames;
        let cooldown_frames = cc.config.cooldown_frames;

        // 워밍업 + 정상 상태 확립 (베이스라인 수립)
        feed_n(&mut cc, window + 60, NORMAL_SERVICE, &mut ts);

        let congested = NORMAL_SERVICE * 2; // score=2.0 > inflate_ratio=1.25
        let mut frame_count = 0u32;
        let mut first_decrease_frame: Option<u32> = None;
        let mut second_decrease_frame: Option<u32> = None;

        // 두 Decrease가 발생할 때까지 지속 혼잡 공급
        for _ in 0..(cooldown_frames + inflate_frames) * 4 {
            let s = ts;
            let e = s + congested;
            let (_, v) = cc.on_frame(s, e, 10_000, INTERVAL_120FPS);
            ts = e + 1;
            frame_count += 1;
            if v == CcVerdict::Decrease {
                if first_decrease_frame.is_none() {
                    first_decrease_frame = Some(frame_count);
                } else if second_decrease_frame.is_none() {
                    second_decrease_frame = Some(frame_count);
                    break;
                }
            }
        }

        let f1 = first_decrease_frame.expect("첫 번째 Decrease가 발생해야 함");
        let f2 = second_decrease_frame.expect("두 번째 Decrease가 발생해야 함");
        let interval = f2 - f1;
        assert!(
            interval >= cooldown_frames + inflate_frames,
            "MD→MD 간격 {interval}이 cooldown({cooldown_frames})+inflate({inflate_frames})={} 미만 (§6 위반)",
            cooldown_frames + inflate_frames
        );
    }

    // T25 — pin: cooldown_frames sanitised to 1..=10_000 (finding #2)
    #[test]
    fn pin_cooldown_frames_sanitised() {
        let cc0 = CcController::new(CcConfig { cooldown_frames: 0, ..CcConfig::from_ceiling(20_000) });
        assert_eq!(cc0.config.cooldown_frames, 1, "cooldown_frames=0 must clamp to 1");

        let cc_max = CcController::new(CcConfig { cooldown_frames: u32::MAX, ..CcConfig::from_ceiling(20_000) });
        assert_eq!(cc_max.config.cooldown_frames, 10_000, "cooldown_frames=u32::MAX must clamp to 10_000");

        let cc5 = CcController::new(CcConfig { cooldown_frames: 5, ..CcConfig::from_ceiling(20_000) });
        assert_eq!(cc5.config.cooldown_frames, 5, "cooldown_frames=5 must pass through");
    }

    // T26 — pin: budget_factor 상한 clamp → product 유한 보장 (finding #4)
    #[test]
    fn pin_budget_factor_large_no_silent_disable() {
        let cfg = CcConfig { budget_factor: f64::MAX, ..CcConfig::from_ceiling(20_000) };
        let cc = CcController::new(cfg);
        assert!(
            cc.config.budget_factor.is_finite() && cc.config.budget_factor <= 1e6,
            "budget_factor must be clamped to <=1e6 after sanitisation; got {}",
            cc.config.budget_factor
        );
        // clamp 후 product도 유한 → 절대 트리거가 묵음 비활성화되지 않음
        let product = cc.config.budget_factor * (u64::MAX as f64);
        assert!(product.is_finite(), "budget_factor * u64::MAX must be finite after clamp");
    }

    // T23 — budget_factor_nonfinite_sanitised
    #[test]
    fn budget_factor_nonfinite_sanitised() {
        let cfg = CcConfig {
            min_kbps: 1_000,
            max_kbps: 20_000,
            window_frames: 60,
            ema_alpha_inv: 8,
            inflate_ratio: 1.25,
            deflate_ratio: 0.90,
            inflate_frames: 8,
            deflate_frames: 16,
            md_factor: 0.85,
            probe_additive_kbps: 500,
            cooldown_frames: 30,
            loss_threshold: 0.10,
            budget_factor: f64::NAN,
        };
        // Must not panic, must sanitise to 1.0
        let cc = CcController::new(cfg);
        assert!(
            cc.config.budget_factor.is_finite() && cc.config.budget_factor > 0.0,
            "budget_factor must be sanitised to a finite positive value"
        );
        assert_eq!(cc.config.budget_factor, 1.0, "NaN budget_factor must sanitise to 1.0");
        // Also verify the controller works normally after sanitisation
        assert_eq!(cc.target_kbps(), 20_000);
    }

    // ── Transport wiring state (CcShared + pure composition helpers) ────────

    // W01 — fresh CcShared is fully inactive: target 0, empty mailbox,
    // unknown frame interval.
    #[test]
    fn shared_new_is_inactive() {
        let s = CcShared::new();
        let g = s.begin_generation();
        assert_eq!(s.target_kbps(), 0);
        assert_eq!(s.take_loss_for(g), None);
        assert_eq!(s.frame_interval_us(), 0);
    }

    // W02 — publish/read roundtrip within a generation; a new generation
    // resets the readable target to inactive.
    #[test]
    fn shared_target_publish_and_generation_reset() {
        let s = CcShared::new();
        let g1 = s.begin_generation();
        s.publish_target_for(g1, 4_000);
        assert_eq!(s.target_kbps(), 4_000);
        let _g2 = s.begin_generation();
        assert_eq!(s.target_kbps(), 0, "new generation must reset the target");
    }

    // W03 — mailbox drain empties exactly one report: 1 → 0, second take None.
    #[test]
    fn shared_loss_take_drains_single_slot() {
        let s = CcShared::new();
        let g = s.begin_generation();
        s.post_loss_for(g, 128);
        assert_eq!(s.take_loss_for(g), Some(0.5));
        assert_eq!(s.take_loss_for(g), None, "a report must never be returned twice");
    }

    // W04 — latest-wins overwrite: two posts between drains keep only the
    // newer report (mailbox: 1 unread → 1 unread, older superseded by design).
    #[test]
    fn shared_loss_post_overwrites_unread() {
        let s = CcShared::new();
        let g = s.begin_generation();
        s.post_loss_for(g, 10);
        s.post_loss_for(g, 200);
        assert_eq!(s.take_loss_for(g), Some(200.0 / 256.0));
        assert_eq!(s.take_loss_for(g), None);
    }

    // W05 — u8 domain endpoints: 0 (no loss, still a report — must not be
    // confused with "empty") and 255 (max fraction) both roundtrip.
    #[test]
    fn shared_loss_domain_endpoints() {
        let s = CcShared::new();
        let g = s.begin_generation();
        s.post_loss_for(g, 0);
        assert_eq!(
            s.take_loss_for(g),
            Some(0.0),
            "loss 0 is a report, not an empty mailbox"
        );
        s.post_loss_for(g, 255);
        assert_eq!(s.take_loss_for(g), Some(255.0 / 256.0));
    }

    // W09 — ghost-writer target invalidation: a publish tagged with a
    // superseded generation must never surface as a live target (the
    // re-setup CC-disable bypass found in review).
    #[test]
    fn shared_stale_generation_publish_reads_inactive() {
        let s = CcShared::new();
        let g1 = s.begin_generation();
        s.publish_target_for(g1, 8_500);
        assert_eq!(s.target_kbps(), 8_500);
        let g2 = s.begin_generation(); // stream re-setup; g1 writer is now a ghost
        s.publish_target_for(g1, 8_500); // ghost keeps publishing
        assert_eq!(s.target_kbps(), 0, "ghost publish must read as inactive");
        s.publish_target_for(g2, 6_000);
        assert_eq!(s.target_kbps(), 6_000);
    }

    // W10 — ghost drains cannot steal the owner's loss report, and stale
    // posts are not delivered to the new owner.
    #[test]
    fn shared_loss_generation_isolation() {
        let s = CcShared::new();
        let g1 = s.begin_generation();
        let g2 = s.begin_generation();
        s.post_loss_for(g2, 64);
        assert_eq!(s.take_loss_for(g1), None, "ghost must not steal the report");
        assert_eq!(s.take_loss_for(g2), Some(0.25), "owner still receives it");
        s.post_loss_for(g1, 200); // stale post is dropped at the write side
        assert_eq!(s.take_loss_for(g2), None, "stale post must not be delivered");
    }

    // W12 — a loss-triggered MD must also cancel accumulated deflate momentum
    // (review finding): with deflate_count at deflate_frames-1, the very next
    // under-budget frame after on_loss_report must NOT probe the target back
    // up in the same breath as the loss decrease.
    #[test]
    fn loss_report_resets_deflate_momentum() {
        let mut cc = ctl_fast(20_000);
        let mut ts = 0u64;
        let window = cc.config.window_frames as u32;
        let deflate_frames = cc.config.deflate_frames;

        feed_n(&mut cc, window, NORMAL_SERVICE, &mut ts);
        // service_us=0 is the reliable deflate trigger (see T08). Stop one
        // frame short of the probe-up threshold.
        let (_, v) = feed_n(&mut cc, deflate_frames - 1, 0, &mut ts);
        assert_ne!(v, CcVerdict::Increase, "precondition: probe must not have fired yet");

        let after_loss = cc.on_loss_report(0.5);
        assert_eq!(after_loss, 17_000, "≥threshold loss must multiplicatively decrease");

        // The frame carrying the loss is still under-budget. Without the
        // deflate reset it would be the deflate_frames-th consecutive deflate
        // frame and immediately reverse the decrease.
        let (t, v) = feed_n(&mut cc, 1, 0, &mut ts);
        assert_ne!(v, CcVerdict::Increase, "loss MD must not be reversed by stale deflate momentum");
        assert_eq!(t, 17_000, "target must hold at the post-loss value");
    }

    // W11 — begin_generation discards an unread loss report (cross-stream
    // clear) and returns strictly increasing generations.
    #[test]
    fn shared_begin_generation_clears_unread_loss() {
        let s = CcShared::new();
        let g1 = s.begin_generation();
        s.post_loss_for(g1, 30);
        let g2 = s.begin_generation();
        assert!(g2 > g1);
        assert_eq!(s.take_loss_for(g2), None, "unread pre-setup loss must be discarded");
    }

    // W06 — frame interval set/get; 0 = unknown stays representable.
    #[test]
    fn shared_frame_interval_roundtrip() {
        let s = CcShared::new();
        s.set_frame_interval_us(8_333);
        assert_eq!(s.frame_interval_us(), 8_333);
        s.set_frame_interval_us(0);
        assert_eq!(s.frame_interval_us(), 0);
    }

    // W07 — composition: 0 = "no signal" passes the other side through;
    // both present → min; both absent → 0 (adaptation fully disabled).
    #[test]
    fn effective_target_composition_table() {
        assert_eq!(effective_target_kbps(0, 0), 0);
        assert_eq!(effective_target_kbps(5_000, 0), 5_000);
        assert_eq!(effective_target_kbps(0, 5_000), 5_000);
        assert_eq!(effective_target_kbps(8_000, 5_000), 5_000);
        assert_eq!(effective_target_kbps(5_000, 8_000), 5_000);
        assert_eq!(effective_target_kbps(u32::MAX, 1), 1);
    }

    // W08 — fps → interval domain sweep: 0 fps and >1M fps disable the
    // absolute budget trigger (0 μs); common rates map exactly.
    #[test]
    fn interval_from_fps_domain() {
        assert_eq!(interval_us_from_fps(0), 0);
        assert_eq!(interval_us_from_fps(1), 1_000_000);
        assert_eq!(interval_us_from_fps(60), 16_666);
        assert_eq!(interval_us_from_fps(120), 8_333);
        assert_eq!(interval_us_from_fps(1_000_000), 1);
        assert_eq!(interval_us_from_fps(2_000_000), 0);
        assert_eq!(interval_us_from_fps(u32::MAX), 0);
    }
}
