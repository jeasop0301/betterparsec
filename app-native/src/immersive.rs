//! Immersive mode state machine — M4 session-ux, native Phase B
//! (mirrors the web immersive semantics: enter = fullscreen confirmed
//! first, then capture; every exit path converges on one teardown).
//!
//! Pure and tick-driven (watchdog.rs / cursor_auto.ts pattern): the shell
//! feeds `(toggle, fullscreen, focused)` every frame and executes the
//! returned actions — `SetFullscreen` maps to an egui viewport command,
//! `Engage`/`Release` to the stream surface's mouse capture
//! (RawInput relative deltas + `ClipCursor`, present.rs/input.rs).
//!
//! Phase B1 scope: immersive ⇒ relative capture, always (the gaming
//! path). Host-authority auto switching inside immersive (CursorShared
//! visibility, web `auto` parity) and the low-level keyboard hook
//! (Win/Alt-Tab capture — the Keyboard Lock analog) are Phase B2.

/// Side effects the shell must execute, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Request the OS window into/out of fullscreen (egui viewport cmd).
    SetFullscreen(bool),
    /// Engage mouse capture: RawInput registration + cursor clip + focus.
    Engage,
    /// Release mouse capture (single teardown for every exit path).
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Off,
    /// Fullscreen requested; capture waits for the OS to confirm both
    /// fullscreen and focus (web parity: never capture un-fullscreened).
    Entering,
    On,
}

/// Tick-driven immersive controller. `on_tick` returns at most two
/// actions; the shell executes them in order.
#[derive(Debug)]
pub struct Immersive {
    state: State,
}

impl Default for Immersive {
    fn default() -> Self {
        Self { state: State::Off }
    }
}

impl Immersive {
    /// Capture currently engaged (shell re-asserts the cursor clip every
    /// frame while true — robust against window moves/resizes).
    pub fn engaged(&self) -> bool {
        self.state == State::On
    }

    /// One shell frame: `toggle` = immersive button clicked this frame,
    /// `fullscreen`/`focused` = current viewport state.
    pub fn on_tick(&mut self, toggle: bool, fullscreen: bool, focused: bool) -> Vec<Action> {
        match self.state {
            State::Off => {
                if toggle {
                    self.state = State::Entering;
                    return vec![Action::SetFullscreen(true)];
                }
                vec![]
            }
            State::Entering => {
                if toggle {
                    // Cancel before capture ever engaged.
                    self.state = State::Off;
                    return vec![Action::SetFullscreen(false)];
                }
                if fullscreen && focused {
                    self.state = State::On;
                    return vec![Action::Engage];
                }
                vec![] // waiting for the OS to confirm
            }
            State::On => {
                if toggle || !focused {
                    // Button exit / focus lost (alt-tab): full teardown.
                    self.state = State::Off;
                    return vec![Action::Release, Action::SetFullscreen(false)];
                }
                if !fullscreen {
                    // Fullscreen already gone (external exit) — release
                    // capture only; no redundant fullscreen command.
                    self.state = State::Off;
                    return vec![Action::Release];
                }
                vec![]
            }
        }
    }

    /// Session teardown (disconnect / watchdog reconnect / surface loss):
    /// force Off and return the actions still owed.
    pub fn reset(&mut self) -> Vec<Action> {
        let actions = match self.state {
            State::Off => vec![],
            State::Entering => vec![Action::SetFullscreen(false)],
            State::On => vec![Action::Release, Action::SetFullscreen(false)],
        };
        self.state = State::Off;
        actions
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_waits_for_fullscreen_then_engages() {
        let mut im = Immersive::default();
        assert_eq!(
            im.on_tick(true, false, true),
            vec![Action::SetFullscreen(true)]
        );
        assert!(!im.engaged());
        // OS hasn't confirmed yet — no capture.
        assert_eq!(im.on_tick(false, false, true), vec![]);
        // Confirmed: engage exactly once.
        assert_eq!(im.on_tick(false, true, true), vec![Action::Engage]);
        assert!(im.engaged());
        assert_eq!(im.on_tick(false, true, true), vec![]);
    }

    #[test]
    fn entering_needs_focus_too() {
        let mut im = Immersive::default();
        im.on_tick(true, false, true);
        assert_eq!(im.on_tick(false, true, false), vec![]);
        assert_eq!(im.on_tick(false, true, true), vec![Action::Engage]);
    }

    #[test]
    fn toggle_cancels_before_capture() {
        let mut im = Immersive::default();
        im.on_tick(true, false, true);
        assert_eq!(
            im.on_tick(true, false, true),
            vec![Action::SetFullscreen(false)]
        );
        assert!(!im.engaged());
        // Fully off: a later fullscreen confirm must not engage.
        assert_eq!(im.on_tick(false, true, true), vec![]);
    }

    #[test]
    fn button_exit_releases_and_leaves_fullscreen() {
        let mut im = Immersive::default();
        im.on_tick(true, true, true);
        im.on_tick(false, true, true);
        assert_eq!(
            im.on_tick(true, true, true),
            vec![Action::Release, Action::SetFullscreen(false)]
        );
        assert!(!im.engaged());
    }

    #[test]
    fn external_fullscreen_loss_releases_without_fullscreen_cmd() {
        let mut im = Immersive::default();
        im.on_tick(true, true, true);
        im.on_tick(false, true, true);
        assert_eq!(im.on_tick(false, false, true), vec![Action::Release]);
    }

    #[test]
    fn focus_loss_is_a_full_teardown() {
        let mut im = Immersive::default();
        im.on_tick(true, true, true);
        im.on_tick(false, true, true);
        assert_eq!(
            im.on_tick(false, true, false),
            vec![Action::Release, Action::SetFullscreen(false)]
        );
        // Refocusing later must not re-engage by itself.
        assert_eq!(im.on_tick(false, true, true), vec![]);
    }

    #[test]
    fn every_exit_path_releases_exactly_once() {
        for exit in ["toggle", "fullscreen", "focus"] {
            let mut im = Immersive::default();
            im.on_tick(true, true, true);
            im.on_tick(false, true, true);
            let actions = match exit {
                "toggle" => im.on_tick(true, true, true),
                "fullscreen" => im.on_tick(false, false, true),
                _ => im.on_tick(false, true, false),
            };
            assert_eq!(
                actions.iter().filter(|a| **a == Action::Release).count(),
                1,
                "exit path {exit}"
            );
            // Machine is silent afterwards.
            assert_eq!(im.on_tick(false, false, false), vec![]);
        }
    }

    #[test]
    fn reset_returns_owed_actions_per_state() {
        let mut off = Immersive::default();
        assert_eq!(off.reset(), vec![]);

        let mut entering = Immersive::default();
        entering.on_tick(true, false, true);
        assert_eq!(entering.reset(), vec![Action::SetFullscreen(false)]);

        let mut on = Immersive::default();
        on.on_tick(true, true, true);
        on.on_tick(false, true, true);
        assert_eq!(
            on.reset(),
            vec![Action::Release, Action::SetFullscreen(false)]
        );
        assert!(!on.engaged());
    }
}
