//! Adaptive FEC redundancy controller — U2 P3 (ROADMAP U2 "적응 비율
//! A/B", docs/design/fec-framing.md §8). Pure/deterministic: maps a
//! per-interval loss estimate to a target redundancy ratio for
//! [`crate::fec::FecEncoder::set_redundancy`]. No clocks, no I/O.
//!
//! Motivation is quantified: `transport-core/src/bin/fec_rig.rs` showed
//! the fixed 20% ratio heals only ~90% of a 5% independent loss and far
//! less under bursts, while 40–50% clears it — so the ratio must track
//! the channel instead of sitting at one value. Overhead is the cost, so
//! the policy is AIMD-shaped, mirroring the ABR bitrate rule ("급락 즉시,
//! 회복 완만"): raise toward a loss-covering target immediately, decay
//! back toward the floor only after a sustained clean streak. Bounds keep
//! overhead sane.
//!
//! The exact constants (`headroom`, `slope`, decay streak) are a
//! Gate-B/C tuning decision; this module ships the *mechanism* (bounded,
//! hysteretic, monotone-in-loss) and a defensible default. Activating it
//! as the encoder's default path is the live A/B gate, not this code.

/// Tuning parameters for [`FecRatioController`]. Percentages are integer
/// "repairs per 100 sources" (so 20 == 1/5 == the current fixed ratio).
#[derive(Debug, Clone, Copy)]
pub struct FecRatioConfig {
    /// Lower bound on the ratio (never drop below this even when clean).
    pub min_pct: u8,
    /// Upper bound (overhead ceiling).
    pub max_pct: u8,
    /// Starting ratio.
    pub initial_pct: u8,
    /// Target ratio for an observed loss `p` is `headroom + slope·p`
    /// (percent). `slope` is a fixed-point ×100 multiplier: `slope = 300`
    /// means 3×. The rig's independent-channel curve needs roughly 3–4×
    /// the loss to approach full recovery, so the default overshoots the
    /// raw loss to leave FEC decode headroom.
    pub headroom_pct: u8,
    pub slope_x100: u16,
    /// Consecutive clean (below `decay_loss_threshold`) intervals required
    /// before the ratio decays one `decay_step_pct` toward `min_pct`.
    pub decay_streak: u16,
    /// Additive decrease applied once `decay_streak` clean intervals pass.
    pub decay_step_pct: u8,
    /// Loss fraction at/below which an interval counts as "clean" for the
    /// decay streak (small non-zero absorbs measurement noise).
    pub decay_loss_threshold: f64,
}

impl Default for FecRatioConfig {
    fn default() -> Self {
        Self {
            min_pct: 5,
            max_pct: 50,
            initial_pct: 20, // today's fixed value — the migration baseline
            headroom_pct: 5,
            slope_x100: 300, // 3× the loss
            decay_streak: 8,
            decay_step_pct: 2,
            decay_loss_threshold: 0.005,
        }
    }
}

impl FecRatioConfig {
    fn sanitised(self) -> Self {
        let min = self.min_pct.min(self.max_pct.max(1));
        let max = self.max_pct.max(min);
        Self {
            min_pct: min,
            max_pct: max,
            initial_pct: self.initial_pct.clamp(min, max),
            headroom_pct: self.headroom_pct,
            slope_x100: self.slope_x100,
            decay_streak: self.decay_streak,
            decay_step_pct: self.decay_step_pct,
            decay_loss_threshold: self.decay_loss_threshold.max(0.0),
        }
    }
}

/// Bounded, hysteretic loss → redundancy-ratio controller.
#[derive(Debug)]
pub struct FecRatioController {
    config: FecRatioConfig,
    current_pct: u8,
    clean_streak: u16,
}

impl FecRatioController {
    pub fn new(config: FecRatioConfig) -> Self {
        let config = config.sanitised();
        Self {
            current_pct: config.initial_pct,
            clean_streak: 0,
            config,
        }
    }

    /// Current ratio as a percentage (repairs per 100 sources).
    pub fn current_pct(&self) -> u8 {
        self.current_pct
    }

    /// Current ratio as an `(numerator, denominator)` pair suitable for
    /// [`crate::fec::FecEncoder::set_redundancy`] (denominator fixed at
    /// 100 so the value fits `u8` and reads as a percentage).
    pub fn current_ratio(&self) -> (u8, u8) {
        (self.current_pct, 100)
    }

    /// The ratio a loss fraction alone would call for (headroom + slope·p),
    /// clamped to `[min_pct, max_pct]`. Exposed for testing the mapping
    /// independent of the hysteresis state.
    pub fn loss_target_pct(&self, loss_fraction: f64) -> u8 {
        let loss_pct = (loss_fraction.clamp(0.0, 1.0) * 100.0).round();
        let scaled = loss_pct * f64::from(self.config.slope_x100) / 100.0;
        let target = f64::from(self.config.headroom_pct) + scaled;
        let target = target.round().clamp(
            f64::from(self.config.min_pct),
            f64::from(self.config.max_pct),
        );
        target as u8
    }

    /// Feed one control-interval loss estimate (fraction 0..1) and return
    /// the updated `(numerator, denominator)` ratio.
    ///
    /// - Loss above the current target → raise immediately to the target
    ///   (fast reaction, no streak required).
    /// - Loss at/below `decay_loss_threshold` → count toward the decay
    ///   streak; once `decay_streak` clean intervals accumulate, step the
    ///   ratio down by `decay_step_pct` and reset the streak.
    /// - In-between (some loss, but the loss-target is not above current)
    ///   → hold, and break the clean streak so a brief clean blip cannot
    ///   race the decay.
    pub fn observe(&mut self, loss_fraction: f64) -> (u8, u8) {
        let target = self.loss_target_pct(loss_fraction);
        let clean = loss_fraction <= self.config.decay_loss_threshold;

        if target > self.current_pct {
            // Raise immediately to cover the observed loss.
            self.current_pct = target;
            self.clean_streak = 0;
        } else if clean {
            self.clean_streak = self.clean_streak.saturating_add(1);
            if self.clean_streak >= self.config.decay_streak {
                self.current_pct = self
                    .current_pct
                    .saturating_sub(self.config.decay_step_pct)
                    .max(self.config.min_pct);
                self.clean_streak = 0;
            }
        } else {
            // Non-trivial loss that the current ratio already covers:
            // hold and reset the streak (no decay while loss persists).
            self.clean_streak = 0;
        }
        self.current_ratio()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> FecRatioController {
        FecRatioController::new(FecRatioConfig::default())
    }

    #[test]
    fn starts_at_initial_within_bounds() {
        let c = ctrl();
        assert_eq!(c.current_pct(), 20);
        assert_eq!(c.current_ratio(), (20, 100));
    }

    #[test]
    fn loss_target_is_headroom_plus_slope_clamped() {
        let c = ctrl(); // headroom 5, slope 3×, min 5, max 50
        assert_eq!(c.loss_target_pct(0.0), 5); // just headroom
        assert_eq!(c.loss_target_pct(0.05), 20); // 5 + 3*5
        assert_eq!(c.loss_target_pct(0.10), 35); // 5 + 3*10
        assert_eq!(c.loss_target_pct(0.20), 50); // 5 + 3*20 = 65 → clamp 50
        assert_eq!(c.loss_target_pct(0.50), 50); // clamp
    }

    #[test]
    fn loss_spike_raises_immediately_in_one_observe() {
        let mut c = ctrl();
        assert_eq!(c.current_pct(), 20);
        // 10% loss → target 35, raised in a single interval.
        c.observe(0.10);
        assert_eq!(c.current_pct(), 35);
        // A worse spike raises further.
        c.observe(0.20);
        assert_eq!(c.current_pct(), 50);
    }

    #[test]
    fn clean_stream_decays_only_after_the_streak() {
        let mut c = ctrl();
        c.observe(0.20); // now at 50
        assert_eq!(c.current_pct(), 50);
        // decay_streak = 8: the first 7 clean intervals hold.
        for _ in 0..7 {
            c.observe(0.0);
            assert_eq!(c.current_pct(), 50, "held before streak elapsed");
        }
        // 8th clean interval steps down by 2.
        c.observe(0.0);
        assert_eq!(c.current_pct(), 48);
    }

    #[test]
    fn decay_floors_at_min_and_never_below() {
        let cfg = FecRatioConfig {
            decay_streak: 1, // decay every clean interval
            decay_step_pct: 10,
            ..Default::default()
        };
        let mut c = FecRatioController::new(cfg);
        // From 20, clean intervals: 20→10→5 (min)→5 (stays).
        c.observe(0.0);
        assert_eq!(c.current_pct(), 10);
        c.observe(0.0);
        assert_eq!(c.current_pct(), 5);
        c.observe(0.0);
        assert_eq!(c.current_pct(), 5, "clamped at min_pct");
    }

    #[test]
    fn a_loss_blip_resets_the_clean_streak() {
        let mut c = ctrl();
        c.observe(0.20); // 50
        for _ in 0..7 {
            c.observe(0.0); // 7 clean, one short of decay
        }
        assert_eq!(c.current_pct(), 50);
        // Non-clean interval that the current ratio already covers: hold,
        // and the streak resets so decay does not fire next interval.
        c.observe(0.10); // target 35 < 50 → hold, streak reset
        assert_eq!(c.current_pct(), 50);
        c.observe(0.0); // streak = 1, not 8
        assert_eq!(c.current_pct(), 50, "streak restarted, no premature decay");
    }

    #[test]
    fn monotone_non_decreasing_in_a_single_rising_sweep() {
        let mut c = ctrl();
        let mut last = c.current_pct();
        for loss_pct in [0u32, 2, 4, 6, 8, 10, 15, 20, 30] {
            c.observe(f64::from(loss_pct) / 100.0);
            assert!(
                c.current_pct() >= last,
                "ratio dropped mid-rise at loss {loss_pct}%: {} < {last}",
                c.current_pct()
            );
            last = c.current_pct();
        }
        assert_eq!(c.current_pct(), 50);
    }

    #[test]
    fn ratio_maps_to_encoder_pair() {
        let mut c = ctrl();
        assert_eq!(c.observe(0.05), (20, 100));
        assert_eq!(c.observe(0.10), (35, 100));
    }

    #[test]
    fn config_sanitises_inverted_bounds_and_out_of_range_initial() {
        let cfg = FecRatioConfig {
            min_pct: 40,
            max_pct: 10, // inverted
            initial_pct: 100,
            ..Default::default()
        };
        let c = FecRatioController::new(cfg);
        // min becomes min(40, max(10,1))=10; max becomes max(10,10)=10;
        // initial clamped into [10,10].
        assert_eq!(c.current_pct(), 10);
    }

    #[test]
    fn sustained_loss_holds_high_without_decaying() {
        let mut c = ctrl();
        c.observe(0.20); // 50
        for _ in 0..20 {
            c.observe(0.08); // steady 8% loss, target 29 < 50 → hold
            assert_eq!(c.current_pct(), 50);
        }
    }
}
