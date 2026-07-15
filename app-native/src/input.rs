//! Input capture — A2 (design D8 Phase A,
//! docs/design/unified-app-architecture.md §4-3).
//!
//! The stream child HWND (present.rs) stops being hit-test transparent
//! once input is enabled and its wndproc feeds Win32 messages through
//! [`translate`] into the session's input channels — the exact wire the
//! web client speaks (`common::input_wire`, Win32 VK codes on the wire,
//! so keyboard translation is a passthrough).
//!
//! A2 scope: absolute mouse (position scaled to the stream reference),
//! buttons with drag capture, high-res wheel, keyboard. The local
//! cursor is hidden over the stream area (WM_SETCURSOR → SetCursor
//! NULL): Sunshine blends the host cursor into the video whenever it
//! is visible, so showing the local arrow too produces a double cursor
//! (field issue #2; cursor-channel.md P1 "cursor:none over video").
//! Relative mouse (RawInput + cursor lock) arrives with the
//! `session-ux` immersive state machine (M4/Phase B).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use client_transport::session::InputSender;
use common::input_wire::InboundPacket;
use moonlight_common::stream::control::{
    KeyAction, KeyFlags, KeyModifiers, MouseButton, MouseButtonAction,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, HTCLIENT, SetCursor, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::VideoShared;
use crate::cursor_icon::ActiveCursor;

/// Attached to the stream child window (GWLP_USERDATA) when a session
/// is running; owned by `StreamSurface`.
pub struct InputCtx {
    pub sender: InputSender,
    /// Stream dimensions source (`VideoShared::dims`, `w << 32 | h`).
    pub video: Arc<VideoShared>,
    /// Cursor the child should show over its client area (0 = hide) —
    /// written by the shell pump (M4 cursor P2, cursor_icon.rs).
    pub cursor: Arc<ActiveCursor>,
}

/// Geometry needed to map client coordinates onto the stream.
#[derive(Clone, Copy)]
struct Viewport {
    client_w: i32,
    client_h: i32,
    stream_w: u16,
    stream_h: u16,
}

fn lo_i16(v: isize) -> i32 {
    (v & 0xFFFF) as u16 as i16 as i32
}

fn hi_i16(v: isize) -> i16 {
    ((v >> 16) & 0xFFFF) as u16 as i16
}

/// WM_SETCURSOR policy (pure, unit-tested): hide the local cursor only
/// over the client area — non-client hits (borders of a future
/// top-level stream window) keep the system cursor.
fn setcursor_hides(lparam: isize) -> bool {
    (lparam as usize & 0xFFFF) as u32 == HTCLIENT
}

/// Rate-limited cursor diagnostics (field issue #2 follow-up): the first
/// few WM_SETCURSOR hits log immediately (proves the handler runs at
/// all), then every 256th (proves it keeps running without flooding).
fn cursor_debug_log(what: &str) {
    use std::sync::atomic::AtomicU32;
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 4 || n.is_multiple_of(256) {
        tracing::debug!(n, "[cursor] {what}");
    }
}

/// Pure Win32-message → wire-packet translation (unit-tested headless).
/// `None` = not an input message / not translatable yet (no stream dims).
fn translate(
    msg: u32,
    wparam: usize,
    lparam: isize,
    vp: Viewport,
    modifiers: KeyModifiers,
) -> Option<InboundPacket> {
    let button = |action: MouseButtonAction, button: MouseButton| {
        Some(InboundPacket::MouseButton { action, button })
    };
    use MouseButtonAction::{Press, Release};
    match msg {
        WM_MOUSEMOVE => {
            if vp.stream_w == 0 || vp.stream_h == 0 || vp.client_w <= 0 || vp.client_h <= 0 {
                return None;
            }
            // Captured drags report coordinates outside the client area.
            let x = lo_i16(lparam).clamp(0, vp.client_w - 1);
            let y = i32::from(hi_i16(lparam)).clamp(0, vp.client_h - 1);
            let sx = (x as i64 * vp.stream_w as i64 / vp.client_w as i64) as i16;
            let sy = (y as i64 * vp.stream_h as i64 / vp.client_h as i64) as i16;
            Some(InboundPacket::MousePosition {
                x: sx,
                y: sy,
                reference_width: vp.stream_w as i16,
                reference_height: vp.stream_h as i16,
            })
        }
        WM_LBUTTONDOWN => button(Press, MouseButton::Left),
        WM_LBUTTONUP => button(Release, MouseButton::Left),
        WM_RBUTTONDOWN => button(Press, MouseButton::Right),
        WM_RBUTTONUP => button(Release, MouseButton::Right),
        WM_MBUTTONDOWN => button(Press, MouseButton::Middle),
        WM_MBUTTONUP => button(Release, MouseButton::Middle),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let b = if (wparam >> 16) & 0xFFFF == 1 {
                MouseButton::X1
            } else {
                MouseButton::X2
            };
            button(
                if msg == WM_XBUTTONDOWN {
                    Press
                } else {
                    Release
                },
                b,
            )
        }
        // Positive wheel delta = away from the user = scroll up, matching
        // the wire's moonlight scroll convention.
        WM_MOUSEWHEEL => Some(InboundPacket::HighResScroll {
            delta_x: 0,
            delta_y: hi_i16(wparam as isize),
        }),
        WM_MOUSEHWHEEL => Some(InboundPacket::HighResScroll {
            delta_x: hi_i16(wparam as isize),
            delta_y: 0,
        }),
        // The wire uses Win32 VK codes — wparam passes straight through.
        WM_KEYDOWN | WM_SYSKEYDOWN => Some(InboundPacket::Key {
            action: KeyAction::Down,
            modifiers,
            key: wparam as u16,
            flags: KeyFlags::empty(),
        }),
        WM_KEYUP | WM_SYSKEYUP => Some(InboundPacket::Key {
            action: KeyAction::Up,
            modifiers,
            key: wparam as u16,
            flags: KeyFlags::empty(),
        }),
        _ => None,
    }
}

fn current_modifiers() -> KeyModifiers {
    let down = |vk: u16| unsafe { GetKeyState(vk as i32) } < 0;
    let mut m = KeyModifiers::empty();
    if down(VK_SHIFT.0) {
        m |= KeyModifiers::SHIFT;
    }
    if down(VK_CONTROL.0) {
        m |= KeyModifiers::CTRL;
    }
    if down(VK_MENU.0) {
        m |= KeyModifiers::ALT;
    }
    if down(VK_LWIN.0) || down(VK_RWIN.0) {
        m |= KeyModifiers::META;
    }
    m
}

/// Applies the shell-published stream cursor (0 = hide — the P1 posture;
/// non-zero = client-rendered host shape, M4 cursor P2).
fn apply_cursor(ctx: &InputCtx) {
    let handle = ctx.cursor.get();
    let cursor = (handle != 0).then_some(windows::Win32::UI::WindowsAndMessaging::HCURSOR(
        handle as *mut core::ffi::c_void,
    ));
    unsafe { SetCursor(cursor) };
}

/// Window-message hook called from the stream surface wndproc (UI
/// thread). `Some(_)` = handled (message consumed — also suppresses the
/// Alt/F10 system-menu default for SYSKEY messages).
pub fn handle(
    ctx: &InputCtx,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    // Single-cursor: the host cursor lives in the video (module doc) —
    // unless the shell pump published a client-rendered shape (M4 cursor
    // P2, `BP_CLIENT_CURSOR=1`): then that HCURSOR is applied instead.
    if msg == WM_SETCURSOR {
        if !setcursor_hides(lparam.0) {
            cursor_debug_log("WM_SETCURSOR non-client — keeping system cursor");
            return None; // DefWindowProc → normal system cursor
        }
        apply_cursor(ctx);
        cursor_debug_log("WM_SETCURSOR client — applied stream cursor");
        return Some(LRESULT(1)); // TRUE: cursor handled, no arrow reset
    }
    // Reinforcement (field report 2026-07-15: arrow still follows on
    // hover): re-assert the stream cursor on every mouse move so anything
    // that reset the thread cursor between moves (parent chrome, other
    // in-process code) is overridden at the next movement — exactly the
    // moments a "following" arrow is visible. WM_MOUSEMOVE still falls
    // through to translate() below.
    if msg == WM_MOUSEMOVE {
        apply_cursor(ctx);
    }

    // Focus and drag-capture side effects first.
    match msg {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => unsafe {
            let _ = SetFocus(Some(hwnd));
            SetCapture(hwnd);
        },
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => unsafe {
            let _ = ReleaseCapture();
        },
        _ => {}
    }

    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    let dims = ctx.video.dims.load(Ordering::Relaxed);
    let vp = Viewport {
        client_w: rect.right - rect.left,
        client_h: rect.bottom - rect.top,
        stream_w: (dims >> 32) as u16,
        stream_h: dims as u16,
    };
    let pkt = translate(msg, wparam.0, lparam.0, vp, current_modifiers())?;
    ctx.sender.send(&pkt);
    Some(LRESULT(0))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const VP: Viewport = Viewport {
        client_w: 200,
        client_h: 100,
        stream_w: 1920,
        stream_h: 1080,
    };

    fn lparam_xy(x: i16, y: i16) -> isize {
        ((y as u16 as isize) << 16) | (x as u16 as isize)
    }

    fn wparam_wheel(delta: i16) -> usize {
        (delta as u16 as usize) << 16
    }

    #[test]
    fn mouse_position_scales_to_stream_reference() {
        match translate(
            WM_MOUSEMOVE,
            0,
            lparam_xy(100, 50),
            VP,
            KeyModifiers::empty(),
        ) {
            Some(InboundPacket::MousePosition {
                x,
                y,
                reference_width,
                reference_height,
            }) => {
                assert_eq!((x, y), (960, 540));
                assert_eq!((reference_width, reference_height), (1920, 1080));
            }
            other => panic!("wrong packet: {other:?}"),
        }
    }

    #[test]
    fn mouse_position_clamps_captured_drag_coords() {
        // Captured drags go negative / past the client edge.
        match translate(
            WM_MOUSEMOVE,
            0,
            lparam_xy(-40, 500),
            VP,
            KeyModifiers::empty(),
        ) {
            Some(InboundPacket::MousePosition { x, y, .. }) => {
                assert_eq!(x, 0);
                assert_eq!(y as i32, 1080_i32 * 99 / 100); // last client row
            }
            other => panic!("wrong packet: {other:?}"),
        }
    }

    #[test]
    fn mouse_move_without_stream_dims_is_dropped() {
        let vp = Viewport {
            stream_w: 0,
            stream_h: 0,
            ..VP
        };
        assert!(translate(WM_MOUSEMOVE, 0, lparam_xy(1, 1), vp, KeyModifiers::empty()).is_none());
    }

    #[test]
    fn buttons_and_xbuttons_map() {
        match translate(WM_LBUTTONDOWN, 0, 0, VP, KeyModifiers::empty()) {
            Some(InboundPacket::MouseButton { action, button }) => {
                assert_eq!(action, MouseButtonAction::Press);
                assert_eq!(button, MouseButton::Left);
            }
            other => panic!("wrong packet: {other:?}"),
        }
        match translate(WM_XBUTTONUP, 2 << 16, 0, VP, KeyModifiers::empty()) {
            Some(InboundPacket::MouseButton { action, button }) => {
                assert_eq!(action, MouseButtonAction::Release);
                assert_eq!(button, MouseButton::X2);
            }
            other => panic!("wrong packet: {other:?}"),
        }
    }

    #[test]
    fn wheel_delta_passes_signed() {
        match translate(
            WM_MOUSEWHEEL,
            wparam_wheel(-120),
            0,
            VP,
            KeyModifiers::empty(),
        ) {
            Some(InboundPacket::HighResScroll { delta_x, delta_y }) => {
                assert_eq!((delta_x, delta_y), (0, -120));
            }
            other => panic!("wrong packet: {other:?}"),
        }
        match translate(
            WM_MOUSEHWHEEL,
            wparam_wheel(120),
            0,
            VP,
            KeyModifiers::empty(),
        ) {
            Some(InboundPacket::HighResScroll { delta_x, delta_y }) => {
                assert_eq!((delta_x, delta_y), (120, 0));
            }
            other => panic!("wrong packet: {other:?}"),
        }
    }

    #[test]
    fn keys_pass_vk_codes_through() {
        match translate(WM_KEYDOWN, 0x41, 0, VP, KeyModifiers::SHIFT) {
            Some(InboundPacket::Key {
                action,
                modifiers,
                key,
                ..
            }) => {
                assert_eq!(action, KeyAction::Down);
                assert_eq!(modifiers, KeyModifiers::SHIFT);
                assert_eq!(key, 0x41);
            }
            other => panic!("wrong packet: {other:?}"),
        }
        match translate(WM_SYSKEYUP, 0x73, 0, VP, KeyModifiers::ALT) {
            Some(InboundPacket::Key { action, key, .. }) => {
                assert_eq!(action, KeyAction::Up);
                assert_eq!(key, 0x73); // F4
            }
            other => panic!("wrong packet: {other:?}"),
        }
    }

    #[test]
    fn unrelated_messages_are_ignored() {
        assert!(
            translate(
                0x0083, /* WM_NCCALCSIZE */
                0,
                0,
                VP,
                KeyModifiers::empty()
            )
            .is_none()
        );
    }

    #[test]
    fn setcursor_hides_only_client_area() {
        use windows::Win32::UI::WindowsAndMessaging::{HTBORDER, HTCAPTION};
        // WM_SETCURSOR lparam: low word = hit-test code.
        assert!(setcursor_hides(HTCLIENT as isize));
        assert!(!setcursor_hides(HTCAPTION as isize));
        assert!(!setcursor_hides(HTBORDER as isize));
        // High word (trigger message) must not affect the decision.
        assert!(setcursor_hides(((0x0200_isize) << 16) | HTCLIENT as isize));
    }
}
