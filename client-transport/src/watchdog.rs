//! Session-UX stall watchdog — Rust mirror of `web/stream/session_ux.ts`
//! (M4, field issue #1). Keep the two in lockstep: same rung order, same
//! untuned defaults, one rung per tick, pause/resume around minimized /
//! hidden phases, `Recovered { stalled_ms }` + full ladder reset on the
//! first frame after a visible stall.
//!
//! Pure and timer-free: the caller (native session shell) drives it with
//! wall-clock milliseconds and executes the returned actions — request an
//! IDR / ICE restart over the signaling WebSocket (`RequestIdr` /
//! `RestartIce` StreamClientMessages) or tear the session down.
//!
//! API shape divergence from TS: `tick`/`frame_received` return
//! `Option<WatchdogAction>` instead of an action array — the machine never
//! emits more than one action per call by construction.

/// Command for the wiring layer; the machine performs no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Show the user-visible stall indicator.
    Stall,
    /// Frames flowed again — hide the indicator.
    Recovered { stalled_ms: u64 },
    /// Ask the host for an IDR (1-based attempt).
    RequestIdr { attempt: u32 },
    /// Ask the host to re-offer with fresh ICE credentials.
    RestartIce,
    /// Terminal: the wiring tears the session down and reconnects.
    Reconnect,
}

/// Ladder thresholds, milliseconds since the last delivered frame.
#[derive(Debug, Clone, Copy)]
pub struct WatchdogConfig {
    /// Stall indicator threshold — episode start.
    pub stall_indicator_ms: u64,
    /// First IDR request.
    pub idr_first_ms: u64,
    /// Interval between repeated IDR requests.
    pub idr_retry_ms: u64,
    /// Total IDR attempts before moving up the ladder.
    pub idr_max_attempts: u32,
    /// ICE restart rung.
    pub ice_restart_ms: u64,
    /// Full reconnect rung (terminal).
    pub reconnect_ms: u64,
}

/// Untuned engineering defaults — MUST match
/// `DEFAULT_WATCHDOG_CONFIG` in `web/stream/session_ux.ts` (Gate B/C
/// tuning pending).
impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            stall_indicator_ms: 1_000,
            idr_first_ms: 2_000,
            idr_retry_ms: 2_000,
            idr_max_attempts: 3,
            ice_restart_ms: 10_000,
            reconnect_ms: 20_000,
        }
    }
}

/// Ladder rungs in strict escalation order. At most ONE rung fires per
/// tick: a suspended/throttled driver can make ticks arrive minutes
/// apart, and a blast of stall→idr→ice→reconnect in a single tick would
/// tear down a session that recovers on the next delivered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rung {
    Stall,
    Idr,
    Ice,
    Reconnect,
}

/// Pure, timer-free stall watchdog (see module docs for the drive
/// contract: `start` → `frame_received`/`tick` → `pause`/`resume` →
/// `stop`). After `Reconnect` fires the machine stays silent until the
/// next `start`.
#[derive(Debug, Default)]
pub struct StallWatchdog {
    config: WatchdogConfig,
    running: bool,
    paused: bool,
    last_frame_ms: u64,
    stall_shown: bool,
    idr_attempts: u32,
    ice_requested: bool,
    reconnect_requested: bool,
}
impl StallWatchdog {
    pub fn new(config: WatchdogConfig) -> Self {
        Self {
            config,
            running: false,
            paused: false,
            last_frame_ms: 0,
            stall_shown: false,
            idr_attempts: 0,
            ice_requested: false,
            reconnect_requested: false,
        }
    }

    /// Whether the stall indicator is currently up (UI readback).
    pub fn stalled(&self) -> bool {
        self.stall_shown
    }

    /// Milliseconds without a frame, for indicator text. 0 when not
    /// running or paused.
    pub fn stalled_ms(&self, now_ms: u64) -> u64 {
        if !self.running || self.paused {
            return 0;
        }
        now_ms.saturating_sub(self.last_frame_ms)
    }

    /// Stream went live: arm the clock. A stream that never delivers a
    /// frame escalates through the same ladder.
    pub fn start(&mut self, now_ms: u64) {
        self.running = true;
        self.paused = false;
        self.last_frame_ms = now_ms;
        self.reset_episode();
    }

    pub fn stop(&mut self) {
        self.running = false;
        self.stall_shown = false;
    }

    /// Window hidden/minimized: hold escalation, keep episode state.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Visible again: restart the clock — the hidden phase proves nothing.
    pub fn resume(&mut self, now_ms: u64) {
        if self.paused {
            self.paused = false;
            self.last_frame_ms = now_ms;
        }
    }

    /// A renderer-delivered frame arrived.
    pub fn frame_received(&mut self, now_ms: u64) -> Option<WatchdogAction> {
        if !self.running {
            return None;
        }
        let action = self.stall_shown.then(|| WatchdogAction::Recovered {
            stalled_ms: now_ms.saturating_sub(self.last_frame_ms),
        });
        self.last_frame_ms = now_ms;
        self.reset_episode();
        action
    }

    /// Periodic driver (e.g. every 250 ms). At most one rung per call.
    pub fn tick(&mut self, now_ms: u64) -> Option<WatchdogAction> {
        if !self.running || self.paused || self.reconnect_requested {
            return None;
        }
        let elapsed = now_ms.saturating_sub(self.last_frame_ms);
        match self.next_rung(elapsed)? {
            Rung::Stall => {
                self.stall_shown = true;
                Some(WatchdogAction::Stall)
            }
            Rung::Idr => {
                self.idr_attempts += 1;
                Some(WatchdogAction::RequestIdr {
                    attempt: self.idr_attempts,
                })
            }
            Rung::Ice => {
                self.ice_requested = true;
                Some(WatchdogAction::RestartIce)
            }
            Rung::Reconnect => {
                self.reconnect_requested = true;
                Some(WatchdogAction::Reconnect)
            }
        }
    }

    fn next_rung(&self, elapsed_ms: u64) -> Option<Rung> {
        let c = &self.config;
        if !self.stall_shown {
            return (elapsed_ms >= c.stall_indicator_ms).then_some(Rung::Stall);
        }
        if self.idr_attempts < c.idr_max_attempts {
            let due = c.idr_first_ms + u64::from(self.idr_attempts) * c.idr_retry_ms;
            if elapsed_ms >= due {
                return Some(Rung::Idr);
            }
        }
        if !self.ice_requested && elapsed_ms >= c.ice_restart_ms {
            return Some(Rung::Ice);
        }
        (elapsed_ms >= c.reconnect_ms).then_some(Rung::Reconnect)
    }

    fn reset_episode(&mut self) {
        self.stall_shown = false;
        self.idr_attempts = 0;
        self.ice_requested = false;
        self.reconnect_requested = false;
    }
}

// ── Tests — ported from tests/session_ux.test.mjs (keep in lockstep) ──────

#[cfg(test)]
mod tests {
    use super::*;

    // Small config so tests read in round numbers (mirror of CFG in the
    // TS suite; identical to the defaults today).
    fn make_dog() -> StallWatchdog {
        let mut dog = StallWatchdog::new(WatchdogConfig::default());
        dog.start(0);
        dog
    }

    /// Drive ticks every `step` through `to`, collecting all actions.
    fn drive(dog: &mut StallWatchdog, to: u64, step: u64, from: u64) -> Vec<WatchdogAction> {
        let mut actions = Vec::new();
        let mut t = from + step;
        while t <= to {
            actions.extend(dog.tick(t));
            t += step;
        }
        actions
    }

    #[test]
    fn frames_flowing_no_actions_not_stalled() {
        let mut dog = make_dog();
        let mut t = 16;
        while t <= 5_000 {
            assert_eq!(dog.frame_received(t), None);
            assert_eq!(dog.tick(t), None);
            t += 16;
        }
        assert!(!dog.stalled());
    }

    #[test]
    fn not_running_tick_and_frame_received_are_silent() {
        let mut dog = StallWatchdog::new(WatchdogConfig::default());
        assert_eq!(dog.frame_received(100), None);
        assert_eq!(dog.tick(10_000), None);
        assert!(!dog.stalled());
    }

    #[test]
    fn full_ladder_fires_in_order_with_250ms_ticks() {
        let mut dog = make_dog();
        let actions = drive(&mut dog, 21_000, 250, 0);
        assert_eq!(
            actions,
            vec![
                WatchdogAction::Stall,
                WatchdogAction::RequestIdr { attempt: 1 },
                WatchdogAction::RequestIdr { attempt: 2 },
                WatchdogAction::RequestIdr { attempt: 3 },
                WatchdogAction::RestartIce,
                WatchdogAction::Reconnect,
            ]
        );
    }

    #[test]
    fn idr_attempts_are_1_based_and_spaced_by_retry() {
        let mut dog = make_dog();
        assert_eq!(dog.tick(1_000), Some(WatchdogAction::Stall));
        assert_eq!(dog.tick(1_999), None);
        assert_eq!(
            dog.tick(2_000),
            Some(WatchdogAction::RequestIdr { attempt: 1 })
        );
        assert_eq!(dog.tick(3_999), None);
        assert_eq!(
            dog.tick(4_000),
            Some(WatchdogAction::RequestIdr { attempt: 2 })
        );
        assert_eq!(
            dog.tick(6_000),
            Some(WatchdogAction::RequestIdr { attempt: 3 })
        );
        // Attempts exhausted: nothing until the ICE rung.
        assert_eq!(dog.tick(8_000), None);
        assert_eq!(dog.tick(10_000), Some(WatchdogAction::RestartIce));
    }

    #[test]
    fn indicator_fires_at_threshold_not_before() {
        let mut dog = make_dog();
        assert_eq!(dog.tick(999), None);
        assert!(!dog.stalled());
        assert_eq!(dog.tick(1_000), Some(WatchdogAction::Stall));
        assert!(dog.stalled());
    }

    #[test]
    fn after_reconnect_the_machine_is_silent_until_restarted() {
        let mut dog = make_dog();
        drive(&mut dog, 21_000, 250, 0);
        assert_eq!(drive(&mut dog, 120_000, 250, 21_000), vec![]);
        // start() re-arms.
        dog.start(120_000);
        assert_eq!(dog.tick(121_000), Some(WatchdogAction::Stall));
    }

    #[test]
    fn a_huge_tick_gap_escalates_one_rung_not_the_whole_ladder() {
        let mut dog = make_dog();
        // First tick after 60 s of throttling: only the indicator fires.
        assert_eq!(dog.tick(60_000), Some(WatchdogAction::Stall));
        // Next tick fires exactly one more rung.
        assert_eq!(
            dog.tick(60_250),
            Some(WatchdogAction::RequestIdr { attempt: 1 })
        );
    }

    #[test]
    fn frame_during_stall_emits_recovered_and_resets_the_ladder() {
        let mut dog = make_dog();
        assert_eq!(dog.tick(1_000), Some(WatchdogAction::Stall));
        assert_eq!(
            dog.tick(2_000),
            Some(WatchdogAction::RequestIdr { attempt: 1 })
        );
        assert_eq!(
            dog.frame_received(2_500),
            Some(WatchdogAction::Recovered { stalled_ms: 2_500 })
        );
        assert!(!dog.stalled());
        // Ladder reset: the next episode starts from the indicator.
        assert_eq!(dog.tick(3_499), None);
        assert_eq!(dog.tick(3_500), Some(WatchdogAction::Stall));
    }

    #[test]
    fn recovered_is_not_emitted_without_a_visible_stall() {
        let mut dog = make_dog();
        assert_eq!(dog.tick(900), None);
        assert_eq!(dog.frame_received(950), None);
    }

    #[test]
    fn stream_that_never_delivers_a_frame_escalates_from_start() {
        let mut dog = StallWatchdog::new(WatchdogConfig::default());
        dog.start(1_000);
        assert_eq!(dog.tick(2_000), Some(WatchdogAction::Stall));
    }

    #[test]
    fn paused_watchdog_does_not_escalate_and_resume_restarts_the_clock() {
        let mut dog = make_dog();
        dog.pause();
        assert_eq!(drive(&mut dog, 30_000, 250, 0), vec![]);
        assert_eq!(dog.stalled_ms(30_000), 0);
        dog.resume(30_000);
        // Clock restarted: indicator needs the full threshold again.
        assert_eq!(dog.tick(30_999), None);
        assert_eq!(dog.tick(31_000), Some(WatchdogAction::Stall));
    }

    #[test]
    fn resume_without_pause_is_a_noop() {
        let mut dog = make_dog();
        dog.resume(900);
        // Clock NOT reset: indicator still due at 1000.
        assert_eq!(dog.tick(1_000), Some(WatchdogAction::Stall));
    }

    #[test]
    fn stop_silences_the_machine_and_clears_the_indicator() {
        let mut dog = make_dog();
        assert_eq!(dog.tick(1_000), Some(WatchdogAction::Stall));
        dog.stop();
        assert!(!dog.stalled());
        assert_eq!(dog.tick(60_000), None);
        assert_eq!(dog.frame_received(60_000), None);
    }

    #[test]
    fn stalled_ms_tracks_elapsed_since_last_frame() {
        let mut dog = make_dog();
        dog.frame_received(400);
        assert_eq!(dog.stalled_ms(1_000), 600);
        // Not running → 0.
        dog.stop();
        assert_eq!(dog.stalled_ms(2_000), 0);
    }

    #[test]
    fn default_config_matches_the_ts_mirror() {
        let c = WatchdogConfig::default();
        assert_eq!(c.stall_indicator_ms, 1_000);
        assert_eq!(c.idr_first_ms, 2_000);
        assert_eq!(c.idr_retry_ms, 2_000);
        assert_eq!(c.idr_max_attempts, 3);
        assert_eq!(c.ice_restart_ms, 10_000);
        assert_eq!(c.reconnect_ms, 20_000);
    }
}
