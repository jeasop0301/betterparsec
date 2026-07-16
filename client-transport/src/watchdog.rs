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
    /// Decoded-output stalled while receive kept progressing: the native
    /// pump must flush the decoder and request a fresh IDR (1-based
    /// attempt; bounded — see [`WatchdogConfig::decode_max_attempts`]).
    DecodeStall { attempt: u32 },
    /// Presented-output stalled while decode kept progressing: the native
    /// pump escalates its own bounded ladder (device recreate → R8
    /// fallback) per attempt; once this machine's own attempt budget is
    /// exhausted it emits [`WatchdogAction::Reconnect`] instead (1-based
    /// attempt; bounded — see [`WatchdogConfig::present_max_attempts`]).
    PresentStall { attempt: u32 },
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
    /// Decode-stage: elapsed ms since the last decoded-output heartbeat
    /// before the first `DecodeStall` fires.
    pub decode_stall_ms: u64,
    /// Decode-stage: interval between repeated `DecodeStall` attempts.
    pub decode_retry_ms: u64,
    /// Decode-stage: attempts before the bounded ladder falls through to
    /// `Reconnect`.
    pub decode_max_attempts: u32,
    /// Present-stage: elapsed ms since the last presented-frame heartbeat
    /// before the first `PresentStall` fires.
    pub present_stall_ms: u64,
    /// Present-stage: interval between repeated `PresentStall` attempts.
    pub present_retry_ms: u64,
    /// Present-stage: attempts (device recreate, R8 fallback) before the
    /// bounded ladder falls through to `Reconnect`. MUST stay in lockstep
    /// with app-native's `PRESENT_STALL_MAX_RECREATES` (currently 3): that
    /// many recreate attempts, then one more attempt that lands as the
    /// egui/R8 `Fallback` rung, before this clock's own ladder exhausts
    /// into `Reconnect` — i.e. this value MUST be
    /// `PRESENT_STALL_MAX_RECREATES + 1` so the fallback rung is reachable
    /// before the supervisor escalates past it.
    pub present_max_attempts: u32,
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
            decode_stall_ms: 2_000,
            decode_retry_ms: 2_000,
            decode_max_attempts: 2,
            present_stall_ms: 2_000,
            present_retry_ms: 2_000,
            present_max_attempts: 4,
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

// ── G004: three-stage supervisor ───────────────────────────────────────────
//
// The receive-stage ladder above (`StallWatchdog`) is unchanged: it is the
// existing, already-shipped stage. `WatchdogSupervisor` composes it with two
// new, independently-clocked downstream stages (decode, present) that only
// arm once their own upstream has produced at least one unit of progress —
// unlike the receive stage, which arms immediately at `start`. Receive
// therefore diagnoses only its own failure mode: it is fed by queue pushes
// (`RxCore::frames_delivered`), so "a stream that never delivers a frame"
// is what it catches — it does NOT cover a decoder/present pump that is
// attached but wedged before producing its first decoded/presented output;
// frames can keep flowing through the queue indefinitely while such a pump
// is dead. Detecting that boot-hang is the native pump's own deadline
// responsibility (e.g. a bounded wait for the first decode/present
// callback after attach), not this supervisor's — decode/present only arm
// on real progress so a pump that never starts stays silent here by
// design (headless/probe consumers that intentionally never decode/present
// must not false-Reconnect). Browser/receive symbol arrival must never
// satisfy decode/present progress: callers MUST drive
// `decode_progress`/`present_progress` only from the native decoder/present
// callbacks (`RxCore::note_decoded_output`/`note_presented` in `capi.rs`),
// never from `frame_received`.
//
// Generation guard: `start` takes the caller's real session lease value
// (`capi::SessionGeneration`, converted with `.value()`) and stores it —
// this is not a self-generated counter, so the field is load-bearing: it
// carries the actual lease identity into `IncidentSnapshot`/
// `CtIncidentSnapshot.generation`, and the genuinely asynchronous
// consumers — `WatchdogStatus`'s decode/present stall latches, polled
// cross-thread by the native pump — are stamped with the same value at
// `start` and drop a latched action whose generation predates the current
// one (see `WatchdogStatus::poll_decode_stall`/`poll_present_stall` in
// `session.rs`). Every action `tick` emits is also stamped with the
// generation it was raised under; a caller that executes actions
// asynchronously (e.g. after crossing an await point) MUST re-check
// `is_current(generation)` immediately before acting — a `stop`+`start`
// cycle between the tick and the side effect means the action is stale and
// must be dropped.

/// Which upstream stage stalled — used for typed action routing and
/// incident reporting (G004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogStage {
    Receive,
    Decode,
    Present,
}

/// One bounded downstream-stage rung: `max_attempts` typed retries, then a
/// single terminal `Reconnect` before the clock goes silent (mirrors
/// `StallWatchdog`'s `reconnect_requested` latch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageRung {
    Attempt(u32),
    Reconnect,
}

/// Independently-clocked downstream stage (decode or present). Pure and
/// timer-free like `StallWatchdog`; see the module-level G004 doc for the
/// arm/generation contract.
#[derive(Debug, Default)]
struct StageClock {
    running: bool,
    paused: bool,
    armed: bool,
    last_progress_ms: u64,
    attempts: u32,
    exhausted: bool,
}

impl StageClock {
    fn start(&mut self) {
        self.running = true;
        self.paused = false;
        self.armed = false;
        self.last_progress_ms = 0;
        self.attempts = 0;
        self.exhausted = false;
    }

    fn stop(&mut self) {
        self.running = false;
        self.armed = false;
    }

    fn pause(&mut self) {
        self.paused = true;
    }

    /// Visible/consuming again: rebase the clock, mirrors
    /// `StallWatchdog::resume`.
    fn resume(&mut self, now_ms: u64) {
        if self.paused {
            self.paused = false;
            self.last_progress_ms = now_ms;
        }
    }

    /// Upstream progress for this stage. Arms the clock on the first call
    /// after `start` and resets the attempt ladder on every call — mirrors
    /// `StallWatchdog::frame_received`'s `reset_episode`.
    fn progress(&mut self, now_ms: u64) {
        if !self.running {
            return;
        }
        self.armed = true;
        self.last_progress_ms = now_ms;
        self.attempts = 0;
        self.exhausted = false;
    }

    /// Heartbeat age for incident reporting; 0 when not running/paused/
    /// unarmed (mirrors `StallWatchdog::stalled_ms`).
    fn age_ms(&self, now_ms: u64) -> u64 {
        if !self.running || self.paused || !self.armed {
            return 0;
        }
        now_ms.saturating_sub(self.last_progress_ms)
    }

    fn tick(
        &mut self,
        now_ms: u64,
        stall_ms: u64,
        retry_ms: u64,
        max_attempts: u32,
    ) -> Option<StageRung> {
        if !self.running || self.paused || !self.armed || self.exhausted {
            return None;
        }
        let elapsed = now_ms.saturating_sub(self.last_progress_ms);
        if self.attempts < max_attempts {
            let due = stall_ms + u64::from(self.attempts) * retry_ms;
            if elapsed >= due {
                self.attempts += 1;
                return Some(StageRung::Attempt(self.attempts));
            }
            return None;
        }
        let due = stall_ms + u64::from(max_attempts) * retry_ms;
        if elapsed >= due {
            self.exhausted = true;
            return Some(StageRung::Reconnect);
        }
        None
    }
}

/// Composes the existing receive-stage `StallWatchdog` with two new
/// downstream stage clocks (decode, present) under one drive contract: see
/// the module-level G004 doc comment for the arm/generation rules.
#[derive(Debug, Default)]
pub struct WatchdogSupervisor {
    config: WatchdogConfig,
    generation: u64,
    receive: StallWatchdog,
    decode: StageClock,
    present: StageClock,
}

impl WatchdogSupervisor {
    pub fn new(config: WatchdogConfig) -> Self {
        Self {
            config,
            generation: 0,
            receive: StallWatchdog::new(config),
            decode: StageClock::default(),
            present: StageClock::default(),
        }
    }

    /// Stream went live: arm the receive stage immediately (a stream that
    /// never delivers is the receive stage's own failure), reset the
    /// decode/present stages to disarmed (they arm on first progress), and
    /// store the caller's real session generation (see the module-level
    /// G004 doc for why this must be the actual `SessionGeneration` lease
    /// value, not a self-generated counter). Returns `generation` unchanged
    /// for convenience.
    pub fn start(&mut self, now_ms: u64, generation: u64) -> u64 {
        self.generation = generation;
        self.receive.start(now_ms);
        self.decode.start();
        self.present.start();
        self.generation
    }

    pub fn stop(&mut self) {
        self.receive.stop();
        self.decode.stop();
        self.present.stop();
    }

    /// Current generation token (see the module-level G004 doc).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether `generation` is still the active one — callers MUST check
    /// this immediately before executing an action tick returned earlier.
    pub fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    /// Window hidden/minimized: hold escalation on all three stages (reuses
    /// the same pause seam the receive stage already had).
    pub fn pause(&mut self) {
        self.receive.pause();
        self.decode.pause();
        self.present.pause();
    }

    /// Visible again: rebase all three stage clocks.
    pub fn resume(&mut self, now_ms: u64) {
        self.receive.resume(now_ms);
        self.decode.resume(now_ms);
        self.present.resume(now_ms);
    }

    /// Receive-stage progress signal ONLY — browser/receive symbol arrival
    /// must never satisfy decode/present progress (see the module-level
    /// G004 doc).
    pub fn frame_received(&mut self, now_ms: u64) -> Option<WatchdogAction> {
        self.receive.frame_received(now_ms)
    }

    /// Decode-stage progress signal — drive this from
    /// `RxCore::decoded_output_count()` deltas (native decoder callback),
    /// never from receive-side symbol arrival.
    pub fn decode_progress(&mut self, now_ms: u64) {
        self.decode.progress(now_ms);
    }

    /// Present-stage progress signal — drive this from
    /// `RxCore::presented_count()` deltas (native present callback), never
    /// from decode/receive-side signals.
    pub fn present_progress(&mut self, now_ms: u64) {
        self.present.progress(now_ms);
    }

    /// Receive-stage indicator readback (UI parity with the pre-G004 API).
    pub fn stalled(&self) -> bool {
        self.receive.stalled()
    }

    /// Heartbeat ages for incident reporting.
    pub fn receive_age_ms(&self, now_ms: u64) -> u64 {
        self.receive.stalled_ms(now_ms)
    }
    pub fn decode_age_ms(&self, now_ms: u64) -> u64 {
        self.decode.age_ms(now_ms)
    }
    pub fn present_age_ms(&self, now_ms: u64) -> u64 {
        self.present.age_ms(now_ms)
    }

    /// At most ONE action per call across all three stages — receive is
    /// checked first (a receive-side root cause is diagnosed/fixed before
    /// downstream stages, which cannot make progress without frames
    /// anyway), then decode, then present. Returns the stage, the action,
    /// and the generation it was raised under (see the module-level G004
    /// doc for why callers must recheck `is_current` before acting).
    pub fn tick(&mut self, now_ms: u64) -> Option<(WatchdogStage, WatchdogAction, u64)> {
        let at_generation = self.generation;
        if let Some(action) = self.receive.tick(now_ms) {
            return Some((WatchdogStage::Receive, action, at_generation));
        }
        let c = &self.config;
        if let Some(rung) = self.decode.tick(
            now_ms,
            c.decode_stall_ms,
            c.decode_retry_ms,
            c.decode_max_attempts,
        ) {
            let action = match rung {
                StageRung::Attempt(attempt) => WatchdogAction::DecodeStall { attempt },
                StageRung::Reconnect => WatchdogAction::Reconnect,
            };
            return Some((WatchdogStage::Decode, action, at_generation));
        }
        if let Some(rung) = self.present.tick(
            now_ms,
            c.present_stall_ms,
            c.present_retry_ms,
            c.present_max_attempts,
        ) {
            let action = match rung {
                StageRung::Attempt(attempt) => WatchdogAction::PresentStall { attempt },
                StageRung::Reconnect => WatchdogAction::Reconnect,
            };
            return Some((WatchdogStage::Present, action, at_generation));
        }
        None
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

// ── G004 supervisor tests ──────────────────────────────────────────────────

#[cfg(test)]
mod supervisor_tests {
    use super::*;

    fn make_sup() -> WatchdogSupervisor {
        let mut sup = WatchdogSupervisor::new(WatchdogConfig::default());
        sup.start(0, 1);
        sup
    }

    #[test]
    fn decode_stage_stalls_independently_while_receive_and_present_progress() {
        let mut sup = make_sup();
        sup.decode_progress(0);
        sup.present_progress(0);
        // Receive/present stay fed throughout; decode never produces
        // output again after its initial arming progress at t=0.
        for t in (0..=6_000u64).step_by(250) {
            assert_eq!(sup.frame_received(t), None);
            sup.present_progress(t);
        }
        // decode_stall_ms=2_000, decode_retry_ms=2_000: first attempt at 2_000.
        assert_eq!(
            sup.tick(2_000),
            Some((
                WatchdogStage::Decode,
                WatchdogAction::DecodeStall { attempt: 1 },
                1
            ))
        );
        assert_eq!(
            sup.tick(4_000),
            Some((
                WatchdogStage::Decode,
                WatchdogAction::DecodeStall { attempt: 2 },
                1
            ))
        );
        // decode_max_attempts=2: exhausted at 6_000 -> bounded Reconnect.
        assert_eq!(
            sup.tick(6_000),
            Some((WatchdogStage::Decode, WatchdogAction::Reconnect, 1))
        );
        // Decode's own clock is silent afterwards (mirrors StallWatchdog's
        // reconnect_requested) — verified directly since ticking again here
        // would otherwise surface unrelated receive/present escalation
        // (neither fed past t=6_000 in this test).
        assert!(sup.decode.exhausted);
    }

    #[test]
    fn present_stage_stalls_independently_while_receive_and_decode_progress() {
        let mut sup = make_sup();
        sup.decode_progress(0);
        sup.present_progress(0);
        for t in (0..=10_000u64).step_by(250) {
            assert_eq!(sup.frame_received(t), None);
            sup.decode_progress(t);
        }
        // present_max_attempts=4 (kept in lockstep with app-native's
        // PRESENT_STALL_MAX_RECREATES=3 so the Fallback rung on the 4th
        // attempt is reachable before this ladder exhausts): four
        // PresentStall rungs at 2_000/4_000/6_000/8_000, then Reconnect.
        assert_eq!(
            sup.tick(2_000),
            Some((
                WatchdogStage::Present,
                WatchdogAction::PresentStall { attempt: 1 },
                1
            ))
        );
        assert_eq!(
            sup.tick(4_000),
            Some((
                WatchdogStage::Present,
                WatchdogAction::PresentStall { attempt: 2 },
                1
            ))
        );
        assert_eq!(
            sup.tick(6_000),
            Some((
                WatchdogStage::Present,
                WatchdogAction::PresentStall { attempt: 3 },
                1
            ))
        );
        assert_eq!(
            sup.tick(8_000),
            Some((
                WatchdogStage::Present,
                WatchdogAction::PresentStall { attempt: 4 },
                1
            ))
        );
        assert_eq!(
            sup.tick(10_000),
            Some((WatchdogStage::Present, WatchdogAction::Reconnect, 1))
        );
    }

    #[test]
    fn decode_and_present_stages_stay_unarmed_until_first_progress() {
        let mut sup = make_sup();
        // Neither stage has produced output yet: no action, ever, until
        // armed — an unarmed stage is not a failure signal (nothing has
        // started using it yet).
        for t in (0..=60_000u64).step_by(1_000) {
            if let Some((stage, ..)) = sup.tick(t) {
                assert_ne!(stage, WatchdogStage::Decode);
                assert_ne!(stage, WatchdogStage::Present);
            }
        }
    }

    #[test]
    fn receive_stall_is_routed_and_reported_before_downstream_stages() {
        let mut sup = make_sup();
        sup.decode_progress(0);
        sup.present_progress(0);
        // Receive stalls first (default stall_indicator_ms=1_000); decode/
        // present would not be due until 2_000 either, but receive always
        // wins the race by priority.
        assert_eq!(
            sup.tick(1_000),
            Some((WatchdogStage::Receive, WatchdogAction::Stall, 1))
        );
    }

    #[test]
    fn pause_rebases_all_three_stage_clocks() {
        let mut sup = make_sup();
        sup.decode_progress(0);
        sup.present_progress(0);
        sup.pause();
        // Nothing escalates while paused, on any stage.
        for t in (0..=30_000u64).step_by(1_000) {
            assert_eq!(sup.tick(t), None);
        }
        assert_eq!(sup.receive_age_ms(30_000), 0);
        assert_eq!(sup.decode_age_ms(30_000), 0);
        assert_eq!(sup.present_age_ms(30_000), 0);
        sup.resume(30_000);
        // Keep receive fed while proving decode/present clocks restarted
        // at the resume point, not at their pre-pause progress time.
        let mut t = 30_250u64;
        while t < 32_000 {
            assert_eq!(sup.frame_received(t), None);
            assert_eq!(sup.tick(t), None);
            t += 250;
        }
        assert_eq!(sup.frame_received(32_000), None);
        assert_eq!(
            sup.tick(32_000),
            Some((
                WatchdogStage::Decode,
                WatchdogAction::DecodeStall { attempt: 1 },
                1
            ))
        );
    }

    #[test]
    fn is_current_detects_a_generation_change_between_two_start_calls() {
        // Primitive-level contract test: `WatchdogSupervisor` is a reusable
        // pure state machine, so `is_current` must correctly age out an
        // action captured under a superseded generation even though
        // `run_session` today calls `start()` at most once per instance
        // (a duplicate `ConnectionComplete` is a terminating protocol
        // error — see flow.rs). The real, reachable async-staleness path
        // (a `WatchdogStatus` decode/present latch recorded under one
        // generation and polled cross-thread after a later `start()`) is
        // covered by `session::tests::watchdog_status_drops_a_stall_latch_recorded_under_a_stale_generation`.
        let mut sup = make_sup();
        let (_, _, action_gen) = sup.tick(1_000).expect("receive stall due at 1_000");
        assert!(sup.is_current(action_gen));
        // A fresh start with a new caller-supplied generation must age out
        // the action captured above.
        let new_gen = sup.start(2_000, 2);
        assert_ne!(new_gen, action_gen);
        assert!(!sup.is_current(action_gen));
        assert!(sup.is_current(new_gen));
    }

    #[test]
    fn stop_silences_all_three_stages() {
        let mut sup = make_sup();
        sup.decode_progress(0);
        sup.present_progress(0);
        sup.stop();
        for t in (0..=10_000u64).step_by(1_000) {
            assert_eq!(sup.tick(t), None);
        }
        assert_eq!(sup.frame_received(10_000), None);
    }

    #[test]
    fn decode_progress_before_running_is_dropped_not_armed() {
        let mut sup = WatchdogSupervisor::new(WatchdogConfig::default());
        // Called before start(): must be dropped, not queued/leaked into
        // arming the stage once it does start.
        sup.decode_progress(0);
        sup.start(0, 1);
        // Keep receive fed throughout; decode must stay silent forever
        // since it never received a real post-start progress signal (an
        // unarmed stage is not itself a failure — the receive ladder
        // already covers "nothing ever arrived").
        for t in (0..=20_000u64).step_by(1_000) {
            assert_eq!(sup.frame_received(t), None);
            if let Some((stage, ..)) = sup.tick(t) {
                assert_ne!(stage, WatchdogStage::Decode);
            }
        }
    }
    #[test]
    fn pause_active_at_start_must_be_reapplied_by_caller_to_survive_start() {
        // `start()` unconditionally clears `paused` on all three stage
        // clocks (a fresh episode's clock must not inherit a stale
        // hidden-phase pause) — this mirrors `run_session`'s
        // `FlowAction::Complete` fix: the caller MUST re-apply `pause()`
        // immediately after `start()` when a pause request is still
        // active, or escalation silently resumes while the window is
        // still minimized. This test proves the fake-clock sequence
        // pause -> start -> (re-)pause -> tick never fires while paused,
        // and that `resume` correctly rebases the clock.
        let mut sup = WatchdogSupervisor::new(WatchdogConfig::default());
        sup.pause();
        let generation = sup.start(0, 1);
        // Caller re-applies the still-active pause request right after
        // start, exactly as `run_session` now does.
        sup.pause();
        for t in (0..=30_000u64).step_by(1_000) {
            assert_eq!(sup.tick(t), None, "paused: no action may fire at t={t}");
        }
        assert_eq!(sup.receive_age_ms(30_000), 0);
        sup.resume(30_000);
        assert_eq!(sup.tick(30_999), None, "not yet due after resume rebase");
        assert_eq!(
            sup.tick(31_000),
            Some((WatchdogStage::Receive, WatchdogAction::Stall, generation))
        );
    }
}
