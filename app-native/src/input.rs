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
    GetClientRect, HTCLIENT, SetCursor, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN,
    WM_XBUTTONUP,
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
    /// Immersive relative-capture flag (M4 Phase B, immersive.rs) —
    /// while set, WM_INPUT raw deltas own mouse movement and the
    /// absolute WM_MOUSEMOVE translation is suppressed.
    pub capture: Arc<CaptureShared>,
}

/// Shell→wndproc shared immersive capture state.
#[derive(Debug, Default)]
pub struct CaptureShared(std::sync::atomic::AtomicBool);

impl CaptureShared {
    pub fn set_relative(&self, on: bool) {
        self.0.store(on, Ordering::Release);
    }

    pub fn relative(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// RAWMOUSE.usFlags bit 0 — absolute-coordinate packet (tablets,
/// injected input). Phase B1 consumes relative packets only.
const MOUSE_MOVE_ABSOLUTE_FLAG: u16 = 0x01;

/// Pure raw-mouse translation (unit-tested): relative packets clamp to
/// the wire's i16 delta; absolute-flagged and zero-motion packets are
/// dropped. Deltas go on the wire unscaled (mickeys), matching
/// moonlight-native semantics — the host injects them as relative
/// motion, so pointer-speed/accel of the client OS never applies.
fn raw_mouse_delta(us_flags: u16, dx: i32, dy: i32) -> Option<(i16, i16)> {
    if us_flags & MOUSE_MOVE_ABSOLUTE_FLAG != 0 {
        return None;
    }
    if dx == 0 && dy == 0 {
        return None;
    }
    Some((
        dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    ))
}

/// Reads one WM_INPUT packet's mouse delta from the given HRAWINPUT
/// lparam. `None` for non-mouse packets, absolute packets, or API
/// failure.
fn read_raw_mouse(lparam: LPARAM) -> Option<(i16, i16)> {
    use windows::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RID_INPUT, RIM_TYPEMOUSE,
    };
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    let got = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut core::ffi::c_void),
            RID_INPUT,
            Some(&mut raw as *mut RAWINPUT as *mut core::ffi::c_void),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if got == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 {
        return None;
    }
    let mouse = unsafe { raw.data.mouse };
    raw_mouse_delta(mouse.usFlags.0, mouse.lLastX, mouse.lLastY)
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
/// non-zero = client-rendered host shape, M4 cursor P2). Immersive
/// relative capture force-hides regardless of the shape slot.
fn apply_cursor(ctx: &InputCtx) {
    let handle = if ctx.capture.relative() {
        0
    } else {
        ctx.cursor.get()
    };
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
        // Immersive relative capture: WM_INPUT owns movement — never
        // translate clipped-cursor moves into absolute positions.
        if ctx.capture.relative() {
            return Some(LRESULT(0));
        }
    }
    // Immersive relative capture: raw deltas → MOUSE_RELATIVE channel.
    if msg == WM_INPUT {
        if ctx.capture.relative()
            && let Some((delta_x, delta_y)) = read_raw_mouse(lparam)
        {
            ctx.sender
                .send(&InboundPacket::MouseMove { delta_x, delta_y });
        }
        return None; // DefWindowProc performs WM_INPUT cleanup
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

    #[test]
    fn raw_mouse_delta_relative_clamps_and_filters() {
        // Relative packets pass through.
        assert_eq!(raw_mouse_delta(0, -2, 300), Some((-2, 300)));
        // Clamp to the wire's i16.
        assert_eq!(
            raw_mouse_delta(0, 100_000, -100_000),
            Some((i16::MAX, i16::MIN))
        );
        // Zero motion (button-only packets) is dropped.
        assert_eq!(raw_mouse_delta(0, 0, 0), None);
        // Absolute-flagged packets (tablets/injected) are dropped.
        assert_eq!(raw_mouse_delta(MOUSE_MOVE_ABSOLUTE_FLAG, 5, 5), None);
        assert_eq!(raw_mouse_delta(MOUSE_MOVE_ABSOLUTE_FLAG | 0x02, 5, 5), None);
    }

    #[test]
    fn capture_shared_roundtrip() {
        let c = CaptureShared::default();
        assert!(!c.relative());
        c.set_relative(true);
        assert!(c.relative());
        c.set_relative(false);
        assert!(!c.relative());
    }
}
