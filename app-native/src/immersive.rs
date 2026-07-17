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
//! path). Phase B2 adds host-authority auto switching inside immersive
//! ([`wants_relative_capture`], mirroring Parsec/Moonlight and the web
//! `auto` mouse mode: relative capture follows whether the *host*
//! cursor is currently hidden, not a fixed always-relative policy) and
//! the low-level keyboard hook (Win/Alt-Tab capture — the Keyboard Lock
//! analog).

/// Host-authority mouse-mode decision (Phase B2; Parsec/Moonlight and
/// web `auto` parity): while immersive is engaged, relative capture
/// mirrors the *host's* reported cursor visibility — hidden means the
/// host (game) has captured the pointer, so the client should too;
/// visible means the host is showing a cursor (menu/desktop), so the
/// client releases to absolute so its cursor tracks the host's exactly.
/// Not engaged ⇒ never relative, regardless of host cursor state.
pub fn wants_relative_capture(engaged: bool, host_cursor_visible: bool) -> bool {
    engaged && !host_cursor_visible
}

/// Per-frame capture verdict while engaged (Phase B2 + focus guard).
///
/// The Alt+Tab/UAC escape hole (field report 2026-07-17): the
/// WM_KILLFOCUS cleanup lives on the stream *child*'s wndproc, so any
/// focus loss that bypasses the child (focus was on the parent chrome
/// window, or the keyboard hook died and a local Alt+Tab switched
/// windows) left `engaged()` true while the shell kept re-asserting
/// `ClipCursor` every repaint — caging the cursor to a background
/// window with no escape. The shell therefore polls the foreground
/// window every frame and folds it into this decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCapture {
    /// One of our windows is foreground and the host cursor is hidden
    /// (game): keep relative capture and re-assert the clip.
    Clip,
    /// Foreground, host cursor visible (menu/desktop): absolute input,
    /// cursor free.
    Unclip,
    /// Neither the stream child nor the chrome window is foreground — a
    /// system key sequence escaped capture: release everything and exit
    /// immersive instead of caging the cursor to a background window.
    ReleaseAndExit,
}

/// Foreground-aware Phase B2 decision, applied every engaged frame.
pub fn frame_capture(our_window_foreground: bool, host_cursor_visible: bool) -> FrameCapture {
    if !our_window_foreground {
        FrameCapture::ReleaseAndExit
    } else if host_cursor_visible {
        FrameCapture::Unclip
    } else {
        FrameCapture::Clip
    }
}

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
    /// Fullscreen requested; one settle frame before engaging capture.
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

    /// One shell frame: `toggle` = immersive button clicked (or the
    /// Ctrl+Alt+Shift+Q escape hatch fired) this frame.
    ///
    /// Capture engages one frame after the fullscreen request rather than
    /// waiting on the OS to *report* fullscreen/focus: that readback
    /// proved unreliable (the window went fullscreen but the reported flag
    /// never flipped, wedging capture in `Entering` forever). The
    /// per-frame cursor re-clip fixes the geometry once fullscreen lands,
    /// and exit is always available via the button or the keyboard-hook
    /// escape hatch, so no focus/fullscreen auto-exit is needed.
    pub fn on_tick(&mut self, toggle: bool) -> Vec<Action> {
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
                // Settle frame elapsed: engage unconditionally.
                self.state = State::On;
                vec![Action::Engage]
            }
            State::On => {
                if toggle {
                    // The only exit: button or Ctrl+Alt+Shift+Q hatch.
                    self.state = State::Off;
                    return vec![Action::Release, Action::SetFullscreen(false)];
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

    fn engage(im: &mut Immersive) {
        // Off -> Entering (request fullscreen) -> On (engage next frame).
        assert_eq!(im.on_tick(true), vec![Action::SetFullscreen(true)]);
        assert!(!im.engaged());
        assert_eq!(im.on_tick(false), vec![Action::Engage]);
        assert!(im.engaged());
    }

    #[test]
    fn enter_then_engages_next_frame() {
        let mut im = Immersive::default();
        engage(&mut im);
        // Steady state is silent.
        assert_eq!(im.on_tick(false), vec![]);
        assert!(im.engaged());
    }

    #[test]
    fn capture_does_not_wait_on_os_readback() {
        // Regression: the OS fullscreen/focus flags used to gate Engage and
        // could wedge Entering forever. A single plain tick after the
        // request must engage regardless of any external state.
        let mut im = Immersive::default();
        im.on_tick(true);
        assert_eq!(im.on_tick(false), vec![Action::Engage]);
        assert!(im.engaged());
    }

    #[test]
    fn toggle_cancels_before_capture() {
        let mut im = Immersive::default();
        im.on_tick(true);
        assert_eq!(im.on_tick(true), vec![Action::SetFullscreen(false)]);
        assert!(!im.engaged());
        // Fully off: a later plain tick must not engage.
        assert_eq!(im.on_tick(false), vec![]);
        assert!(!im.engaged());
    }

    #[test]
    fn button_or_hatch_exit_releases_and_leaves_fullscreen() {
        let mut im = Immersive::default();
        engage(&mut im);
        assert_eq!(
            im.on_tick(true),
            vec![Action::Release, Action::SetFullscreen(false)]
        );
        assert!(!im.engaged());
        // Silent afterwards.
        assert_eq!(im.on_tick(false), vec![]);
    }

    #[test]
    fn exit_releases_exactly_once() {
        let mut im = Immersive::default();
        engage(&mut im);
        let actions = im.on_tick(true);
        assert_eq!(actions.iter().filter(|a| **a == Action::Release).count(), 1);
        assert_eq!(im.on_tick(false), vec![]);
    }

    #[test]
    fn reset_returns_owed_actions_per_state() {
        let mut off = Immersive::default();
        assert_eq!(off.reset(), vec![]);

        let mut entering = Immersive::default();
        entering.on_tick(true);
        assert_eq!(entering.reset(), vec![Action::SetFullscreen(false)]);

        let mut on = Immersive::default();
        on.on_tick(true);
        on.on_tick(false);
        assert_eq!(
            on.reset(),
            vec![Action::Release, Action::SetFullscreen(false)]
        );
        assert!(!on.engaged());
    }

    #[test]
    fn wants_relative_capture_follows_host_authority() {
        // (engaged, host_cursor_visible) -> want_relative
        let cases = [
            (true, true, false),
            (true, false, true),
            (false, true, false),
            (false, false, false),
        ];
        for (engaged, host_cursor_visible, want) in cases {
            assert_eq!(
                wants_relative_capture(engaged, host_cursor_visible),
                want,
                "engaged={engaged} host_cursor_visible={host_cursor_visible}"
            );
        }
    }

    #[test]
    fn frame_capture_releases_on_any_focus_loss_regardless_of_host_cursor() {
        use FrameCapture::*;
        // (our_window_foreground, host_cursor_visible) -> verdict
        let cases = [
            (true, false, Clip),
            (true, true, Unclip),
            (false, false, ReleaseAndExit),
            (false, true, ReleaseAndExit),
        ];
        for (foreground, host_cursor_visible, want) in cases {
            assert_eq!(
                frame_capture(foreground, host_cursor_visible),
                want,
                "foreground={foreground} host_cursor_visible={host_cursor_visible}"
            );
        }
    }
}
