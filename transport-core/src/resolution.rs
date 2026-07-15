//! Dynamic resolution ladder controller — M3 efficiency lever (ROADMAP M3,
//! docs/design/quality-efficiency-audit.md). Pure/deterministic: maps a
//! per-interval available-bandwidth estimate to a target rung in a
//! resolution ladder for the encoder + client upscale path. No clocks, no
//! I/O.
//!
//! Motivation: under bandwidth congestion, encoding at a lower resolution
//! and letting the client upscale beats holding resolution and starving
//! the encoder into blocky, low-bitrate artifacts (see
//! quality-efficiency-audit.md). This mirrors the ABR/CC bitrate rule
//! ("급락 즉시, 회복 완만" — drop fast, recover slow): a shortfall against
//! the *current* rung's need steps down immediately, while a step up
//! requires a sustained clean streak against the *next-higher* rung's
//! need so the controller does not oscillate around a marginal link.
//!
//! The ladder rungs, per-rung `min_kbps` figures, headroom, and
//! `raise_streak` are all Gate-B/C tuning decisions; this module ships the
//! *mechanism* (bounded, hysteretic, monotone-in-bandwidth) and a
//! defensible default, not the final numbers. Activating this as the
//! encoder's live resolution driver is the A/B gate, not this code.

/// Tuning parameters for [`DynResolutionController`]. `rungs` is ordered
/// high→low resolution; index 0 is the highest rung. `min_kbps[i]` is the
/// minimum available bandwidth needed to sustain `rungs[i]` at the target
/// fps without starving the encoder into blocky artifacts.
#[derive(Debug, Clone)]
pub struct ResolutionLadderConfig {
    /// Resolution rungs as `(width, height)`, ordered high→low.
    pub rungs: Vec<(u16, u16)>,
    /// Per-rung minimum kbps needed at the target fps, same length/order
    /// as `rungs`.
    pub min_kbps: Vec<u32>,
    /// Rung index to start the controller at (clamped into range by
    /// [`ResolutionLadderConfig::sanitised`]).
    pub initial_rung: usize,
    /// Drop a rung immediately when `available_kbps < min_kbps[current] *
    /// (1 - lower_headroom_pct / 100)`.
    pub lower_headroom_pct: u8,
    /// Consecutive intervals with `available_kbps` at/above the
    /// next-higher rung's `min_kbps` required before stepping up.
    pub raise_streak: u16,
}

impl Default for ResolutionLadderConfig {
    /// Sensible 60fps ladder. These bitrate figures are Gate-B tunables,
    /// not gospel — see the module-level doc comment.
    fn default() -> Self {
        Self {
            rungs: vec![
                (3840, 2160),
                (2560, 1440),
                (1920, 1080),
                (1600, 900),
                (1280, 720),
                (960, 540),
                (854, 480),
            ],
            min_kbps: vec![35_000, 20_000, 10_000, 7_000, 5_000, 3_000, 2_000],
            initial_rung: 2, // start at 1080p
            lower_headroom_pct: 10,
            raise_streak: 8,
        }
    }
}

impl ResolutionLadderConfig {
    /// Guarantees `rungs` and `min_kbps` are non-empty and the same
    /// length (truncating to the shorter of the two if mismatched), and
    /// clamps `initial_rung` and `lower_headroom_pct` into valid ranges.
    /// Falls back to [`ResolutionLadderConfig::default`]'s ladder if
    /// either list is empty after truncation.
    fn sanitised(mut self) -> Self {
        let len = self.rungs.len().min(self.min_kbps.len());
        self.rungs.truncate(len);
        self.min_kbps.truncate(len);
        if self.rungs.is_empty() || self.min_kbps.is_empty() {
            let d = Self::default();
            self.rungs = d.rungs;
            self.min_kbps = d.min_kbps;
        }
        let max_idx = self.rungs.len().saturating_sub(1);
        Self {
            initial_rung: self.initial_rung.min(max_idx),
            lower_headroom_pct: self.lower_headroom_pct.min(100),
            raise_streak: self.raise_streak,
            rungs: self.rungs,
            min_kbps: self.min_kbps,
        }
    }
}

/// Bounded, hysteretic available-bandwidth → resolution-rung controller.
#[derive(Debug)]
pub struct DynResolutionController {
    config: ResolutionLadderConfig,
    current_rung: usize,
    good_streak: u16,
}

impl DynResolutionController {
    pub fn new(config: ResolutionLadderConfig) -> Self {
        let config = config.sanitised();
        let current_rung = config.initial_rung;
        Self {
            config,
            current_rung,
            good_streak: 0,
        }
    }

    /// Current target resolution.
    pub fn current(&self) -> (u16, u16) {
        self.config.rungs[self.current_rung]
    }

    /// Current rung index (0 = highest resolution).
    pub fn current_rung(&self) -> usize {
        self.current_rung
    }

    fn min_kbps_at(&self, rung: usize) -> u32 {
        self.config.min_kbps[rung]
    }

    /// Feed one control-interval available-bandwidth estimate (kbps) and
    /// return the updated target resolution.
    ///
    /// - `available_kbps` below the current rung's headroom-adjusted
    ///   `min_kbps` → step down one rung immediately (fast reaction, no
    ///   streak required), resetting the good streak.
    /// - Otherwise, if a higher rung exists and `available_kbps` already
    ///   covers that higher rung's `min_kbps` → count toward the raise
    ///   streak; once it reaches `raise_streak`, step up one rung and
    ///   reset the streak.
    /// - Otherwise (holding within the current rung's band, or not yet
    ///   enough to justify raising) → hold and reset the streak so a
    ///   brief good blip cannot race the raise.
    pub fn observe(&mut self, available_kbps: u32) -> (u16, u16) {
        let current_min = self.min_kbps_at(self.current_rung);
        let lower_bound =
            current_min - (current_min as u64 * self.config.lower_headroom_pct as u64 / 100) as u32;

        if available_kbps < lower_bound {
            self.current_rung = (self.current_rung + 1).min(self.config.rungs.len() - 1);
            self.good_streak = 0;
        } else if self.current_rung > 0 && available_kbps >= self.min_kbps_at(self.current_rung - 1)
        {
            self.good_streak = self.good_streak.saturating_add(1);
            if self.good_streak >= self.config.raise_streak {
                self.current_rung -= 1;
                self.good_streak = 0;
            }
        } else {
            self.good_streak = 0;
        }
        self.current()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> DynResolutionController {
        DynResolutionController::new(ResolutionLadderConfig::default())
    }

    #[test]
    fn starts_at_configured_initial_rung() {
        let c = ctrl();
        assert_eq!(c.current_rung(), 2);
        assert_eq!(c.current(), (1920, 1080));
    }

    #[test]
    fn starts_at_a_custom_initial_rung() {
        let cfg = ResolutionLadderConfig {
            initial_rung: 0,
            ..Default::default()
        };
        let c = DynResolutionController::new(cfg);
        assert_eq!(c.current_rung(), 0);
        assert_eq!(c.current(), (3840, 2160));
    }

    #[test]
    fn sudden_drop_steps_down_one_rung_per_observe() {
        let mut c = ctrl(); // 1080p, min 10_000, headroom 10% → lower_bound 9000
        assert_eq!(c.current_rung(), 2);
        c.observe(4_000); // well below 900p's min too, but only steps one rung
        assert_eq!(c.current_rung(), 3);
        assert_eq!(c.current(), (1600, 900));
        c.observe(4_000); // 900p min 7000, headroom → 6300; 4000 < 6300
        assert_eq!(c.current_rung(), 4);
        assert_eq!(c.current(), (1280, 720));
    }

    #[test]
    fn sustained_low_holds_at_the_floor_never_below() {
        let mut c = ctrl();
        for _ in 0..20 {
            c.observe(100); // starves everything
        }
        assert_eq!(c.current_rung(), 6);
        assert_eq!(c.current(), (854, 480));
    }

    #[test]
    fn recovery_raises_only_after_raise_streak_clean_intervals() {
        let mut c = ctrl();
        c.observe(100); // drop to 900p (rung 3)
        assert_eq!(c.current_rung(), 3);
        // Next-higher rung (rung 2, 1080p) needs 10_000; feed enough to
        // qualify for raise.
        for _ in 0..7 {
            c.observe(25_000);
            assert_eq!(c.current_rung(), 3, "held before streak elapsed");
        }
        c.observe(25_000); // 8th consecutive good interval → raise
        assert_eq!(c.current_rung(), 2);
    }

    #[test]
    fn a_dip_resets_the_good_streak() {
        let mut c = ctrl();
        c.observe(100); // rung 3 (900p)
        for _ in 0..7 {
            c.observe(25_000); // 7 good, one short of streak
        }
        assert_eq!(c.current_rung(), 3);
        c.observe(8_000); // covers current rung but not the next-higher → hold, reset streak
        assert_eq!(c.current_rung(), 3);
        c.observe(25_000); // streak = 1, not 8
        assert_eq!(c.current_rung(), 3, "streak restarted, no premature raise");
    }

    #[test]
    fn monotone_within_a_single_dropping_sweep() {
        let mut c = ctrl();
        let mut last = c.current_rung();
        for kbps in [9_000u32, 6_000, 4_500, 2_500, 1_500, 100] {
            c.observe(kbps);
            assert!(
                c.current_rung() >= last,
                "rung improved mid-drop at {kbps}kbps: {} < {last}",
                c.current_rung()
            );
            last = c.current_rung();
        }
    }

    #[test]
    fn clamps_at_the_top_rung() {
        let cfg = ResolutionLadderConfig {
            initial_rung: 0,
            ..Default::default()
        };
        let mut c = DynResolutionController::new(cfg);
        for _ in 0..20 {
            c.observe(100_000); // plenty of bandwidth, already at the top
        }
        assert_eq!(c.current_rung(), 0);
        assert_eq!(c.current(), (3840, 2160));
    }

    #[test]
    fn clamps_at_the_bottom_rung_and_does_not_panic() {
        let mut c = ctrl();
        for _ in 0..10 {
            c.observe(0); // repeated drops must not underflow past rung 6
        }
        assert_eq!(c.current_rung(), 6);
    }

    #[test]
    fn config_sanitises_mismatched_length_inputs() {
        let cfg = ResolutionLadderConfig {
            rungs: vec![(1920, 1080), (1280, 720), (854, 480)],
            min_kbps: vec![10_000, 5_000], // shorter than rungs
            initial_rung: 5,               // out of range after truncation
            ..Default::default()
        };
        let c = DynResolutionController::new(cfg);
        // Truncated to len 2: [(1920,1080),(1280,720)] / [10_000, 5_000].
        assert_eq!(c.current_rung(), 1, "initial_rung clamped to max index");
        assert_eq!(c.current(), (1280, 720));
    }

    #[test]
    fn config_sanitises_empty_inputs_by_falling_back_to_default_ladder() {
        let cfg = ResolutionLadderConfig {
            rungs: vec![],
            min_kbps: vec![],
            ..Default::default()
        };
        let c = DynResolutionController::new(cfg);
        assert_eq!(c.current(), (1920, 1080));
    }

    #[test]
    fn sustained_marginal_bandwidth_holds_without_raising() {
        let mut c = ctrl();
        c.observe(100); // rung 3 (900p), min 7_000
        assert_eq!(c.current_rung(), 3);
        for _ in 0..20 {
            // Enough for 900p (>=6300 headroom-adjusted) but short of
            // 1440p's 20_000 need, so never raises.
            c.observe(7_500);
            assert_eq!(c.current_rung(), 3);
        }
    }
}
