//! Adaptive bitrate (ABR) controller — betterparsec feature #1.
//!
//! Turns the browser's REMB (Receiver Estimated Maximum Bitrate) congestion
//! estimate into a smoothed target bitrate. Philosophy (Parsec BUD-like):
//! latency > fps > quality — react **down fast** on congestion, recover **up
//! slowly**, never exceed the configured ceiling, never fall below a usable
//! floor.
//!
//! This module is intentionally pure/deterministic so it is unit-tested without
//! a live stream. The WebRTC RTCP loop publishes its output through an atomic
//! target; a separate apply task gates and sends the capability-checked Sunshine
//! `0x5506` extension. The protocol has no ACK, so the client reports only
//! `sent_unacknowledged`; encoder application remains a host-log or future ACK
//! concern. See `streamer/src/bitrate_apply.rs` and docs/ROADMAP.md M2.

/// Fraction of the receiver's REMB estimate we actually target when the network
/// (not the configured ceiling) is the binding constraint, leaving headroom for
/// probing and packet overhead. REMB is a *maximum* estimate; targeting 100% of
/// it invites the very congestion we are trying to avoid.
const DEFAULT_HEADROOM: f64 = 0.9;

/// Packet-loss fraction (from RTCP Receiver Reports) at or above which the link
/// is treated as congested and the target is cut. Below this, loss is treated as
/// noise and left to the REMB path. The 10% knee mirrors the loss-based half of
/// the Google Congestion Control (GCC) algorithm.
const HIGH_LOSS_FRACTION: f64 = 0.10;

/// Multiplicative-decrease aggressiveness on high loss: `target *= 1 - K*loss`.
/// `K = 0.5` matches GCC's loss-based controller (halve the rate at 100% loss).
const LOSS_DECREASE_K: f64 = 0.5;

/// Bounds and reaction shape for [`AbrController`].
#[derive(Debug, Clone, Copy)]
pub struct AbrConfig {
    /// Upper bound — the configured/permitted stream bitrate. The target never
    /// exceeds this. Sanitised to `>= 1`.
    pub ceiling_kbps: u32,
    /// Lower bound — the worst-case still-usable bitrate. The target never drops
    /// below this, so a pathological (e.g. zero) estimate degrades quality
    /// instead of stalling the encoder. Sanitised to `1..=ceiling`.
    pub floor_kbps: u32,
    /// Additive per-observation step when recovering upward (gradual increase).
    pub increase_step_kbps: u32,
    /// Fraction of the REMB estimate treated as usable, in `(0.0, 1.0]`.
    pub headroom: f64,
}

impl AbrConfig {
    /// Derive a sane config from just the configured stream bitrate (the ceiling).
    ///
    /// Floor = 10% of ceiling but at least 500 kbps (and never above the
    /// ceiling); recovery reaches the ceiling in ~20 observations (~20 s at the
    /// ~1 Hz REMB cadence), at least 250 kbps per step.
    pub fn from_ceiling(ceiling_kbps: u32) -> Self {
        let ceiling_kbps = ceiling_kbps.max(1);
        let floor_kbps = (ceiling_kbps / 10).max(500).min(ceiling_kbps);
        let increase_step_kbps = (ceiling_kbps / 20).max(250);
        Self {
            ceiling_kbps,
            floor_kbps,
            increase_step_kbps,
            headroom: DEFAULT_HEADROOM,
        }
    }

    /// Clamp all fields into their valid ranges so the controller cannot be
    /// constructed into an invariant-violating state (e.g. floor > ceiling).
    fn sanitised(self) -> Self {
        let ceiling_kbps = self.ceiling_kbps.max(1);
        Self {
            ceiling_kbps,
            floor_kbps: self.floor_kbps.clamp(1, ceiling_kbps),
            increase_step_kbps: self.increase_step_kbps.max(1),
            headroom: if self.headroom.is_finite() && self.headroom > 0.0 {
                self.headroom.min(1.0)
            } else {
                DEFAULT_HEADROOM
            },
        }
    }
}

/// Smooths REMB estimates into a target bitrate. See module docs.
#[derive(Debug, Clone)]
pub struct AbrController {
    config: AbrConfig,
    current_kbps: u32,
}

impl AbrController {
    /// Starts optimistic — at the ceiling — and adapts down as REMB arrives.
    pub fn new(config: AbrConfig) -> Self {
        let config = config.sanitised();
        Self {
            current_kbps: config.ceiling_kbps,
            config,
        }
    }

    /// The current target bitrate (kbps).
    #[allow(dead_code)] // Retained for deterministic tests and diagnostics.
    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    /// Feed one REMB estimate (kbps); returns the new target (kbps).
    ///
    /// Down-fast / up-slow, always within `[floor, ceiling]`:
    /// - If the estimate is at or above the ceiling, the network can carry the
    ///   full configured bitrate, so we target the ceiling (no headroom penalty).
    /// - Otherwise the network is the constraint: target `headroom * estimate`.
    /// - Below the current target → snap down immediately; above → rise by one
    ///   additive step (never past the estimate or the ceiling).
    ///
    /// A zero or non-finite-derived estimate clamps the target to the floor — it
    /// never yields 0 (which would stall the encoder) nor is read as "unlimited".
    pub fn observe(&mut self, observed_kbps: u32) -> u32 {
        let usable = if observed_kbps >= self.config.ceiling_kbps {
            self.config.ceiling_kbps
        } else {
            (observed_kbps as f64 * self.config.headroom) as u32
        };
        let bounded = usable.clamp(self.config.floor_kbps, self.config.ceiling_kbps);

        self.current_kbps = if bounded < self.current_kbps {
            // Congestion: drop immediately to the (bounded) estimate.
            bounded
        } else {
            // Headroom available: rise gradually, never past the estimate/ceiling.
            self.current_kbps
                .saturating_add(self.config.increase_step_kbps)
                .min(bounded)
                .max(self.config.floor_kbps)
        };
        self.current_kbps
    }

    /// Feed one packet-loss observation (fraction in `[0.0, 1.0]`, typically an
    /// RTCP Receiver Report's `fraction_lost / 256`); returns the new target
    /// (kbps).
    ///
    /// Loss only ever pulls the target **down**, never up — recovery stays with
    /// the REMB path in [`Self::observe`]. This is the fast "react down hard on
    /// congestion" safety cut of the latency > fps > quality philosophy: REMB's
    /// bandwidth estimate lags a sudden link degradation, but loss spikes
    /// immediately, so a loss-triggered multiplicative decrease shaves the target
    /// before the queue builds and frames get mangled.
    ///
    /// Below [`HIGH_LOSS_FRACTION`] the loss is treated as noise and the target
    /// is unchanged. Non-finite or out-of-range input is sanitised to a no-op;
    /// this never panics, and the target never drops below the floor nor to zero.
    pub fn observe_loss(&mut self, loss_fraction: f64) -> u32 {
        let loss = if loss_fraction.is_finite() {
            loss_fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };

        if loss >= HIGH_LOSS_FRACTION {
            let reduced = (self.current_kbps as f64 * (1.0 - LOSS_DECREASE_K * loss)) as u32;
            // `reduced <= current <= ceiling`, so only the floor bound can bite.
            self.current_kbps = reduced.clamp(self.config.floor_kbps, self.config.ceiling_kbps);
        }
        self.current_kbps
    }
}

/// Minimum spacing between ordinary runtime bitrate control messages (ms).
/// Rate limiting avoids control churn and repeated encoder reconfiguration.
/// Emergency decreases bypass this interval so congestion response stays fast.
pub(crate) const MIN_APPLY_INTERVAL_MS: u64 = 900;

/// Minimum relative change (`|target-last|/last`) worth sending. Smaller drift
/// is left for the encoder's own rate control to absorb.
const APPLY_HYSTERESIS_FRAC: f64 = 0.10;

/// A decrease this large bypasses the normal send interval. Holding a severe
/// downward correction would let the transport queue grow during congestion.
const EMERGENCY_DECREASE_FRAC: f64 = 0.20;

/// Decides WHEN the continuously-smoothed ABR target ([`AbrController`]) is worth
/// pushing to the Sunshine encoder over the control channel (feature #1 "path A",
/// packet 0x5506). The gate suppresses churn: it emits a value only when it
/// differs from the last-sent one by at least [`APPLY_HYSTERESIS_FRAC`] and the
/// interval has elapsed, except for emergency decreases. The protocol has no
/// acknowledgement, so this state never claims the encoder applied the value.
///
/// Pure/deterministic: the caller supplies a monotonic millisecond timestamp, so
/// it is unit-tested without a clock. State is owned by the single apply task (not
/// shared across threads), so no locking is needed; the *shared* signal it consumes
/// is the `AtomicU32` ABR target read with an atomic load.
#[derive(Debug, Clone)]
pub struct ApplyGate {
    last_sent_kbps: u32,
    last_sent_ms: u64,
    has_baseline: bool,
}

impl Default for ApplyGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplyGate {
    pub fn new() -> Self {
        Self {
            last_sent_kbps: 0,
            last_sent_ms: 0,
            has_baseline: false,
        }
    }

    /// Returns `Some(kbps)` when `target_kbps` should be pushed to the host now,
    /// recording it as the new last-sent value; `None` to hold.
    ///
    /// - A zero target (no estimate yet) never applies.
    /// - The first non-zero target always applies.
    /// - Otherwise: send a decrease of at least 20% immediately. For other
    ///   changes, hold until [`MIN_APPLY_INTERVAL_MS`] has elapsed, then send
    ///   only if the relative change is at least [`APPLY_HYSTERESIS_FRAC`].
    ///
    /// `now_ms` is a caller-supplied monotonic timestamp; going backwards is
    /// treated as "no time elapsed" (holds), never a panic.
    #[allow(dead_code)] // Convenience wrapper retained for deterministic gate tests.
    pub fn should_send(&mut self, target_kbps: u32, now_ms: u64) -> Option<u32> {
        let candidate = self.next_candidate(target_kbps, now_ms)?;
        self.record_sent_unacknowledged(candidate, now_ms);
        Some(candidate)
    }

    /// Returns the next worthwhile bitrate without recording it as sent.
    ///
    /// Runtime callers must use this two-phase form and call
    /// [`Self::record_sent_unacknowledged`] only after the client library queues
    /// the request. The `0x5506` extension has no acknowledgement, so this must
    /// never be described as proof of encoder reconfiguration.
    pub(crate) fn next_candidate(&self, target_kbps: u32, now_ms: u64) -> Option<u32> {
        if target_kbps == 0 {
            return None;
        }
        if !self.has_baseline {
            return Some(target_kbps);
        }
        let last = self.last_sent_kbps.max(1) as f64;
        let delta_frac = (target_kbps as f64 - self.last_sent_kbps as f64).abs() / last;
        let emergency_decrease =
            target_kbps < self.last_sent_kbps && delta_frac >= EMERGENCY_DECREASE_FRAC;
        if !emergency_decrease && now_ms.saturating_sub(self.last_sent_ms) < MIN_APPLY_INTERVAL_MS {
            return None;
        }
        if delta_frac < APPLY_HYSTERESIS_FRAC {
            return None;
        }
        Some(target_kbps)
    }

    pub(crate) fn record_sent_unacknowledged(&mut self, kbps: u32, now_ms: u64) {
        self.record(kbps, now_ms);
    }

    /// Seeds the gate with the bitrate already supplied during stream startup,
    /// avoiding a redundant runtime control message (and possible keyframe).
    pub(crate) fn seed_initial_bitrate(&mut self, kbps: u32, now_ms: u64) {
        if kbps > 0 {
            self.record(kbps, now_ms);
        }
    }

    fn record(&mut self, kbps: u32, now_ms: u64) {
        self.last_sent_kbps = kbps;
        self.last_sent_ms = now_ms;
        self.has_baseline = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A generous, round ceiling to make the arithmetic in assertions obvious.
    // from_ceiling(20000) => floor 2000, step 1000, headroom 0.9.
    fn ctl() -> AbrController {
        AbrController::new(AbrConfig::from_ceiling(20000))
    }

    #[test]
    fn starts_optimistic_at_ceiling() {
        assert_eq!(ctl().current_kbps(), 20000);
    }

    #[test]
    fn from_ceiling_derives_sane_bounds() {
        let c = AbrConfig::from_ceiling(20000);
        assert_eq!(c.ceiling_kbps, 20000);
        assert_eq!(c.floor_kbps, 2000); // 10%
        assert_eq!(c.increase_step_kbps, 1000); // 1/20
    }

    #[test]
    fn zero_estimate_clamps_to_floor_never_zero() {
        // The classic dangerous boundary: 0 must NOT mean "stall to 0 bitrate"
        // and must NOT be read as "no limit". It means "worst case → floor".
        let mut c = ctl();
        let t = c.observe(0);
        assert_eq!(t, 2000); // the floor
        assert!(t > 0);
    }

    #[test]
    fn huge_estimate_never_exceeds_ceiling() {
        let mut c = ctl();
        for _ in 0..100 {
            c.observe(u32::MAX);
        }
        assert_eq!(c.current_kbps(), 20000);
        assert!(c.current_kbps() <= 20000);
    }

    #[test]
    fn estimate_at_ceiling_targets_full_ceiling_no_headroom_penalty() {
        let mut c = ctl();
        assert_eq!(c.observe(20000), 20000);
    }

    #[test]
    fn congestion_drops_fast_with_headroom() {
        let mut c = ctl(); // starts at 20000
        // 8000 < ceiling → usable = 0.9 * 8000 = 7200, snapped down immediately.
        assert_eq!(c.observe(8000), 7200);
    }

    #[test]
    fn recovery_is_gradual_not_instant() {
        let mut c = ctl();
        c.observe(0); // drop to floor 2000
        assert_eq!(c.current_kbps(), 2000);
        // Plenty of bandwidth now, but we rise by one step (1000), not jump.
        assert_eq!(c.observe(u32::MAX), 3000);
        assert_eq!(c.observe(u32::MAX), 4000);
        // ...and eventually converge to the ceiling.
        for _ in 0..50 {
            c.observe(u32::MAX);
        }
        assert_eq!(c.current_kbps(), 20000);
    }

    #[test]
    fn floor_above_ceiling_is_sanitised() {
        // Misconfiguration: floor > ceiling must not violate floor <= ceiling.
        let mut c = AbrController::new(AbrConfig {
            ceiling_kbps: 1000,
            floor_kbps: 5000,
            increase_step_kbps: 250,
            headroom: 0.9,
        });
        assert!(c.current_kbps() <= 1000 && c.current_kbps() >= 1);
        let t = c.observe(0);
        assert!((1..=1000).contains(&t));
    }

    #[test]
    fn zero_ceiling_is_sanitised_no_panic() {
        let mut c = AbrController::new(AbrConfig::from_ceiling(0));
        assert!(c.current_kbps() >= 1);
        assert!(c.observe(0) >= 1); // never 0, never panics
    }

    #[test]
    fn non_finite_or_bad_headroom_falls_back() {
        let mut c = AbrController::new(AbrConfig {
            ceiling_kbps: 10000,
            floor_kbps: 1000,
            increase_step_kbps: 500,
            headroom: f64::NAN,
        });
        // With sane default headroom, an 8000 estimate → 7200, not a NaN-derived value.
        assert_eq!(c.observe(8000), 7200);
    }

    #[test]
    fn invariant_holds_over_arbitrary_sequence() {
        let mut c = ctl();
        let seq = [
            0u32,
            500,
            100_000,
            3000,
            3001,
            2999,
            u32::MAX,
            1,
            20000,
            19999,
            0,
            7500,
        ];
        for &s in seq.iter().cycle().take(500) {
            let t = c.observe(s);
            assert!(
                (2000..=20000).contains(&t),
                "target {t} out of [floor,ceiling] for estimate {s}"
            );
        }
    }

    // --- loss-based reaction (observe_loss) ---

    #[test]
    fn loss_below_threshold_is_noop() {
        let mut c = ctl(); // at ceiling 20000
        // 5% loss < 10% knee → treated as noise, target unchanged.
        assert_eq!(c.observe_loss(0.05), 20000);
        assert_eq!(c.current_kbps(), 20000);
    }

    #[test]
    fn high_loss_cuts_multiplicatively() {
        let mut c = ctl(); // at ceiling 20000
        // 20% loss ≥ knee → target *= 1 - 0.5*0.20 = 0.90 → 18000.
        assert_eq!(c.observe_loss(0.20), 18000);
    }

    #[test]
    fn full_loss_halves_but_not_below_floor() {
        let mut c = ctl(); // at ceiling 20000, floor 2000
        // 100% loss → *0.5 → 10000 (still above the 2000 floor).
        assert_eq!(c.observe_loss(1.0), 10000);
    }

    #[test]
    fn repeated_high_loss_converges_to_floor_never_zero() {
        let mut c = ctl(); // floor 2000
        for _ in 0..200 {
            c.observe_loss(1.0);
        }
        assert_eq!(c.current_kbps(), 2000); // pinned at floor, not 0
        assert!(c.current_kbps() > 0);
    }

    #[test]
    fn non_finite_loss_is_noop_no_panic() {
        let mut c = ctl();
        assert_eq!(c.observe_loss(f64::NAN), 20000);
        assert_eq!(c.observe_loss(f64::INFINITY), 20000);
        assert_eq!(c.observe_loss(f64::NEG_INFINITY), 20000);
        assert_eq!(c.current_kbps(), 20000);
    }

    #[test]
    fn negative_or_over_one_loss_is_sanitised() {
        let mut c = ctl();
        // Negative → clamped to 0 → below knee → no-op.
        assert_eq!(c.observe_loss(-0.5), 20000);
        // >1.0 → clamped to 1.0 → behaves as full loss → *0.5.
        assert_eq!(c.observe_loss(5.0), 10000);
    }

    #[test]
    fn loss_cut_then_remb_recovers_gradually() {
        let mut c = ctl();
        c.observe_loss(1.0); // 20000 -> 10000
        assert_eq!(c.current_kbps(), 10000);
        // Recovery is via the REMB path, one additive step (1000) at a time.
        assert_eq!(c.observe(u32::MAX), 11000);
        assert_eq!(c.observe(u32::MAX), 12000);
    }

    #[test]
    fn loss_at_floor_stays_at_floor() {
        let mut c = ctl();
        c.observe(0); // snap to floor 2000
        assert_eq!(c.current_kbps(), 2000);
        // Further high loss cannot push below the floor.
        assert_eq!(c.observe_loss(1.0), 2000);
    }

    #[test]
    fn mixed_remb_and_loss_invariant_holds() {
        let mut c = ctl();
        for i in 0..500u32 {
            // Interleave REMB estimates and loss observations.
            let t = if i % 2 == 0 {
                c.observe((i.wrapping_mul(137)) % 30000)
            } else {
                c.observe_loss(((i % 20) as f64) / 20.0) // 0.0 ..= 0.95
            };
            assert!(
                (2000..=20000).contains(&t),
                "target {t} out of [floor,ceiling] at step {i}"
            );
        }
    }

    // --- runtime control-message gate ---

    #[test]
    fn apply_gate_zero_target_never_sends() {
        let mut g = ApplyGate::new();
        assert_eq!(g.should_send(0, 0), None);
        assert_eq!(g.should_send(0, 10_000), None);
    }

    #[test]
    fn apply_gate_first_nonzero_target_sends() {
        let mut g = ApplyGate::new();
        assert_eq!(g.should_send(8000, 0), Some(8000));
    }

    #[test]
    fn apply_gate_rate_limits_within_interval() {
        let mut g = ApplyGate::new();
        assert_eq!(g.should_send(8000, 0), Some(8000)); // first always sends
        // A large increase still waits for MIN_APPLY_INTERVAL_MS (900).
        assert_eq!(g.should_send(20000, 500), None);
        assert_eq!(g.should_send(20000, 899), None);
    }

    #[test]
    fn apply_gate_sends_after_interval_if_change_significant() {
        let mut g = ApplyGate::new();
        g.should_send(8000, 0);
        // 8000 -> 10000 is +25% (>=10%), and 1000 >= 900 interval.
        assert_eq!(g.should_send(10000, 1000), Some(10000));
    }

    #[test]
    fn apply_gate_holds_subthreshold_change_after_interval() {
        let mut g = ApplyGate::new();
        g.should_send(10000, 0);
        // 10000 -> 10500 is +5% (<10%): held even though the interval elapsed.
        assert_eq!(g.should_send(10500, 2000), None);
        // A later 10000 -> 11000 (+10%) sends (last-sent still 10000).
        assert_eq!(g.should_send(11000, 4000), Some(11000));
    }

    #[test]
    fn apply_gate_threshold_boundary_sends_at_exactly_10pct() {
        let mut g = ApplyGate::new();
        g.should_send(10000, 0);
        assert_eq!(g.should_send(11000, 1000), Some(11000)); // exactly +10%
    }

    #[test]
    fn apply_gate_backwards_clock_holds_never_panics() {
        let mut g = ApplyGate::new();
        g.should_send(10000, 5000);
        // now_ms < last_sent_ms → saturating_sub = 0 → within interval → hold.
        assert_eq!(g.should_send(20000, 100), None);
    }

    #[test]
    fn apply_gate_emergency_decrease_bypasses_interval() {
        let mut g = ApplyGate::new();
        g.should_send(10000, 0);
        assert_eq!(g.should_send(5000, 100), Some(5000));
    }
}
