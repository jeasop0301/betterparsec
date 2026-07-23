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

use std::fs::OpenOptions;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use client_transport::session::InputSender;
use common::input_wire::InboundPacket;
use moonlight_common::stream::control::{
    KeyAction, KeyFlags, KeyModifiers, MouseButton, MouseButtonAction,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Security::{
    GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
};
use windows::Win32::System::Pipes::{
    GetNamedPipeServerProcessId, PIPE_NOWAIT, SetNamedPipeHandleState,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, MAPVK_VSC_TO_VK_EX, MapVirtualKeyW, ReleaseCapture, SetCapture,
    SetFocus, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_OEM_3, VK_Q,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetClientRect, GetMessageW, HTCLIENT, KBDLLHOOKSTRUCT,
    LLKHF_ALTDOWN, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW, SetCursor,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_INPUT, WM_KEYDOWN,
    WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP,
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
pub struct CaptureShared {
    relative: AtomicBool,
    /// Set by the keyboard hook's Ctrl+Alt+Shift+Q escape hatch (Phase
    /// B2, below); the shell tick consumes it via
    /// [`Self::take_exit_requested`]. While relative capture is
    /// engaged the cursor is clipped to the stream child (present.rs),
    /// so the sidebar "Exit immersive" button is unreachable — this is
    /// the only way out.
    exit_requested: AtomicBool,
    /// Set by the hard-disconnect hotkey (Ctrl+Alt+`, Parsec parity) —
    /// hook or wndproc path; the shell consumes it once per frame and
    /// tears the whole session down (same path as the Disconnect
    /// button), so a fullscreen stream can never trap the user.
    disconnect_requested: AtomicBool,
    /// True for the WHOLE immersive session (Engage → Release), unlike
    /// `relative` which the host-authority auto-switch toggles per frame
    /// (host cursor visible ⇒ relative off). The keyboard hook and the
    /// wndproc escape/focus-loss paths gate on THIS — otherwise Alt+Tab/
    /// Win capture silently dies whenever the host shows its cursor
    /// (field report: Alt+Tab switched CLIENT windows during immersive).
    keyboard_capture: AtomicBool,
}

impl CaptureShared {
    pub fn set_relative(&self, on: bool) {
        self.relative.store(on, Ordering::Release);
    }

    pub fn relative(&self) -> bool {
        self.relative.load(Ordering::Acquire)
    }

    /// Requests immersive exit (keyboard hook, UI thread via the LL
    /// hook's own thread — same process, no cross-thread sync beyond
    /// the atomic).
    pub fn request_exit(&self) {
        self.exit_requested.store(true, Ordering::Release);
    }

    /// Consumes a pending exit request; `false` once already taken this
    /// tick. The shell calls this once per frame (main.rs).
    pub fn take_exit_requested(&self) -> bool {
        self.exit_requested.swap(false, Ordering::AcqRel)
    }

    /// Requests a full session disconnect (Ctrl+Alt+` — hook thread or
    /// wndproc, same-process atomic).
    pub fn request_disconnect(&self) {
        self.disconnect_requested.store(true, Ordering::Release);
    }

    /// Consumes a pending disconnect request; the shell calls this once
    /// per frame and folds it into the Disconnect-button path.
    pub fn take_disconnect_requested(&self) -> bool {
        self.disconnect_requested.swap(false, Ordering::AcqRel)
    }

    /// Immersive-session-scoped keyboard capture (see field doc): set
    /// true on Engage, false on every Release path.
    pub fn set_keyboard_capture(&self, on: bool) {
        self.keyboard_capture.store(on, Ordering::Release);
    }

    pub fn keyboard_capture(&self) -> bool {
        self.keyboard_capture.load(Ordering::Acquire)
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

/// Sends key-up to the host for the modifiers/keys most prone to sticking
/// when immersive capture tears down. A redundant up for a key that was not
/// down is a no-op on the host. The result is deliberately observable: callers
/// that end ownership must disconnect rather than silently leave a stuck key.
pub fn release_sticky_keys(sender: &InputSender) -> bool {
    let packets = [
        VK_MENU, VK_LMENU, VK_RMENU, VK_CONTROL, VK_SHIFT, VK_LWIN, VK_RWIN, VK_TAB, VK_Q,
    ]
    .into_iter()
    .map(|vk| InboundPacket::Key {
        action: KeyAction::Up,
        modifiers: KeyModifiers::empty(),
        key: vk.0,
        flags: KeyFlags::empty(),
    })
    .collect::<Vec<_>>();
    sender.send_batch(&packets)
}

/// The exact ordered Alt+Tab sequence used by the fallback remap and mirrored
/// by brokered driver events. When Shift is physically held, it is carried in
/// the Tab modifiers but never pressed or released synthetically.
fn alt_tab_chord(shift_held: bool) -> Vec<(KeyAction, u16, KeyModifiers)> {
    let mut mods = KeyModifiers::ALT;
    if shift_held {
        mods |= KeyModifiers::SHIFT;
    }
    vec![
        (KeyAction::Down, VK_MENU.0, KeyModifiers::ALT),
        (KeyAction::Down, VK_TAB.0, mods),
        (KeyAction::Up, VK_TAB.0, mods),
        (KeyAction::Up, VK_MENU.0, KeyModifiers::empty()),
    ]
}

/// The hook-independent Ctrl+Tab fallback's ownership state. Ctrl reaches the
/// host before Tab, so the first remapped Tab temporarily lifts it, then the
/// matched Tab-up restores it only when the physical Ctrl key remains down.
#[derive(Debug, Default)]
struct CtrlTabRemap {
    tab_down: bool,
    ctrl_suppressed: bool,
}

#[derive(Debug, PartialEq)]
enum CtrlTabRemapAction {
    Pass,
    Consume(Vec<(KeyAction, u16, KeyModifiers)>),
}

impl CtrlTabRemap {
    fn handle(
        &mut self,
        captured: bool,
        msg: u32,
        vk: u16,
        ctrl_held: bool,
        alt_held: bool,
        shift_held: bool,
    ) -> CtrlTabRemapAction {
        if vk == VK_CONTROL.0 && matches!(msg, WM_KEYUP | WM_SYSKEYUP) {
            self.ctrl_suppressed = false;
            return CtrlTabRemapAction::Pass;
        }

        if vk != VK_TAB.0 {
            return CtrlTabRemapAction::Pass;
        }

        if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && captured && ctrl_held && !alt_held {
            let mut packets = Vec::new();
            if !self.ctrl_suppressed {
                packets.push((KeyAction::Up, VK_CONTROL.0, KeyModifiers::empty()));
                self.ctrl_suppressed = true;
            }
            packets.extend(alt_tab_chord(shift_held));
            self.tab_down = true;
            return CtrlTabRemapAction::Consume(packets);
        }

        if matches!(msg, WM_KEYUP | WM_SYSKEYUP) && self.tab_down {
            self.tab_down = false;
            let mut packets = Vec::new();
            if self.ctrl_suppressed && ctrl_held {
                packets.push((KeyAction::Down, VK_CONTROL.0, KeyModifiers::CTRL));
            }
            self.ctrl_suppressed = false;
            return CtrlTabRemapAction::Consume(packets);
        }

        CtrlTabRemapAction::Pass
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

static CTRL_TAB_REMAP: OnceLock<Mutex<CtrlTabRemap>> = OnceLock::new();

fn ctrl_tab_remap() -> &'static Mutex<CtrlTabRemap> {
    CTRL_TAB_REMAP.get_or_init(|| Mutex::new(CtrlTabRemap::default()))
}

fn reset_ctrl_tab_remap() {
    ctrl_tab_remap()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .reset();
}

/// Pure predicate for the hook-independent immersive escape in [`handle`]:
/// Ctrl+Alt+Q (Shift optional) on a key-down while immersive keyboard capture
/// is engaged. Mirrors the LL-hook combo so a failed hook install cannot trap
/// the user.
fn is_wndproc_escape(
    captured: bool,
    msg: u32,
    vk: u16,
    ctrl: bool,
    alt: bool,
    _shift: bool,
) -> bool {
    captured && matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && vk == VK_Q.0 && ctrl && alt
}

/// Pure predicate for the session hard-disconnect hotkey Ctrl+Alt+`
/// (`VK_OEM_3` — Parsec parity): key-down with Ctrl+Alt held,
/// shift-agnostic, and NOT gated on capture — it must work in fullscreen
/// immersive AND in a plain windowed session whenever the stream child
/// has focus, hook installed or not. The shell folds the request into
/// the Disconnect-button teardown, so a fullscreen stream can never
/// trap the user.
fn is_disconnect_combo(msg: u32, vk: u16, ctrl: bool, alt: bool) -> bool {
    matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && vk == VK_OEM_3.0 && ctrl && alt
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

    // Genuine focus loss while captured (system dialog, UAC, or another
    // forced focus transition): release the cursor clip immediately. eframe
    // may stop repainting an unfocused fullscreen window, so deferring this
    // cleanup to the next shell tick can leave the client cursor trapped.
    if msg == WM_KILLFOCUS && ctx.capture.keyboard_capture() {
        reset_ctrl_tab_remap();
        if !release_sticky_keys(&ctx.sender) {
            ctx.capture.request_disconnect();
        }
        ctx.capture.set_relative(false);
        ctx.capture.set_keyboard_capture(false);
        crate::present::release_mouse_capture_global();
        ctx.capture.request_exit();
        return None; // DefWindowProc still does its normal kill-focus work
    }
    // Hook-independent escape (see is_wndproc_escape): works whenever the
    // captured child has focus, so a failed keyboard-hook install cannot
    // trap the user.
    if is_wndproc_escape(
        ctx.capture.keyboard_capture(),
        msg,
        wparam.0 as u16,
        unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0,
        unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0,
        unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0,
    ) {
        ctx.capture.request_exit();
        reset_ctrl_tab_remap();
        if !release_sticky_keys(&ctx.sender) {
            ctx.capture.request_disconnect();
        }
        return Some(LRESULT(0)); // consume Q; do not forward it to the host
    }
    // Hard disconnect (Ctrl+Alt+`): tears the whole session down via the
    // shell (same as the Disconnect button) — the always-available way
    // out of a fullscreen stream, Parsec-style.
    if is_disconnect_combo(
        msg,
        wparam.0 as u16,
        unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0,
        unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0,
    ) {
        release_sticky_keys(&ctx.sender);
        reset_ctrl_tab_remap();
        ctx.capture.request_disconnect();
        return Some(LRESULT(0)); // consume ` — never forward it
    }
    // Hook-independent host task switch: Ctrl+Tab is a deliberate fallback
    // only when the LL hook cannot own real Alt+Tab. The remap owns both Tab
    // edges, including repeats, and reconciles the Ctrl state already sent to
    // the host.
    let broker_active = is_broker_active();
    if !broker_active {
        let remap_action = ctrl_tab_remap()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .handle(
                ctx.capture.keyboard_capture(),
                msg,
                wparam.0 as u16,
                unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0,
                unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0,
                unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0,
            );
        if let CtrlTabRemapAction::Consume(packets) = remap_action {
            let packets = packets
                .into_iter()
                .map(|(action, key, modifiers)| InboundPacket::Key {
                    action,
                    modifiers,
                    key,
                    flags: KeyFlags::empty(),
                })
                .collect::<Vec<_>>();
            if !ctx.sender.send_batch(&packets) {
                tracing::warn!(
                    "remote Ctrl+Tab chord queue saturated — disconnecting to avoid stuck keys"
                );
                reset_ctrl_tab_remap();
                release_sticky_keys(&ctx.sender);
                ctx.capture.request_disconnect();
            }
            return Some(LRESULT(0));
        }
    }
    if broker_active && matches!(msg, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP) {
        // The driver ring is the sole remote keyboard producer. Leave the
        // local message untouched so Windows still handles local safety keys.
        return None;
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

// ── Keyboard Lock (Phase B2, WH_KEYBOARD_LL) ────────────────────────────────
//
// Immersive relative capture clips the cursor to the stream child, so the
// sidebar "Exit immersive" button is unreachable. A WH_KEYBOARD_LL hook owns
// Alt and Win key edges while captured. Owning the complete Alt+Tab chord in
// one ordered path prevents Windows from opening its local task switcher and
// prevents split wndproc/hook delivery from corrupting the remote chord.

/// Outcome of [`hook_decision`] for one low-level keyboard event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    /// Let Windows (and, after `CallNextHookEx`, the app's own
    /// WM_KEYDOWN/UP path in [`handle`]) process the key normally.
    Pass,
    /// Windows must never see this key: forward it to the host on the
    /// wire and swallow it here (`CallNextHookEx` is skipped).
    SwallowForward,
    /// The Ctrl+Alt+Q escape hatch fired.
    ExitImmersive,
    /// The Ctrl+Alt+` hard-disconnect hotkey fired (Parsec parity).
    Disconnect,
}

fn is_alt_key(vk: u32) -> bool {
    vk == VK_MENU.0 as u32 || vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32
}

fn tracked_alt_state(vk: u32, key_up: bool, previous: bool) -> bool {
    if is_alt_key(vk) { !key_up } else { previous }
}

/// Pure Keyboard Lock decision table (unit-tested, no Win32).
///
/// `alt_down` carries whichever extra-modifier condition matters for
/// `vk`: Alt state for `VK_TAB`, the full Ctrl+Alt+Shift combo for `VK_Q`,
/// or Ctrl+Alt for `VK_OEM_3`. The caller combines the hook event's
/// `LLKHF_ALTDOWN` with `GetAsyncKeyState`, since `GetKeyState` in a low-level
/// hook can lag the event that is still in flight. Rules:
/// - `capture_on == false` ⇒ [`HookAction::Pass`] (hook installed but
///   immersive not engaged — e.g. a race during teardown).
/// - Alt and Win key edges ⇒ [`HookAction::SwallowForward`].
/// - `VK_TAB` while Alt is held ⇒ [`HookAction::SwallowForward`].
/// - `VK_Q` key-down with `alt_down` (Ctrl+Alt satisfied) ⇒
///   [`HookAction::ExitImmersive`].
/// - Everything else ⇒ [`HookAction::Pass`] — the wndproc's WM_KEYDOWN
///   path ([`handle`]) already forwards normal keys; reporting them
///   here too would double-send.
pub fn hook_decision(vk: u32, alt_down: bool, key_up: bool, capture_on: bool) -> HookAction {
    if !capture_on {
        return HookAction::Pass;
    }
    if vk == VK_Q.0 as u32 {
        return if !key_up && alt_down {
            HookAction::ExitImmersive
        } else {
            HookAction::Pass
        };
    }
    if vk == VK_OEM_3.0 as u32 {
        // Ctrl+Alt+` hard disconnect (Parsec parity). `alt_down` carries
        // the Ctrl+Alt condition (shift-agnostic) for this vk.
        return if !key_up && alt_down {
            HookAction::Disconnect
        } else {
            HookAction::Pass
        };
    }
    let is_system_modifier = is_alt_key(vk) || vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32;
    let is_alt_tab = vk == VK_TAB.0 as u32 && alt_down;
    if is_system_modifier || is_alt_tab {
        HookAction::SwallowForward
    } else {
        HookAction::Pass
    }
}

const BROKER_PIPE: &str = r"\\.\pipe\BetterParsec\input-v1";
const BROKER_HEADER_SIZE: usize = 12;
const BROKER_EVENT_SIZE: usize = 32;
const BROKER_MAX_PAYLOAD: usize = 64 * 1024;
const BROKER_KIND_ARM: u16 = 1;
const BROKER_KIND_DISARM: u16 = 2;
const BROKER_KIND_EVENTS: u16 = 3;
const BROKER_KIND_STATUS: u16 = 4;
const DRIVER_KEY_BREAK: u16 = 0x0001;
const DRIVER_KEY_E0: u16 = 0x0002;
const DRIVER_KEY_E1: u16 = 0x0004;

struct BrokerState {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct BrokerRouter {
    ctrl: bool,
    right_ctrl: bool,
    shift: bool,
    right_shift: bool,
    left_alt: bool,
    right_alt: bool,
    left_meta: bool,
    right_meta: bool,
    pending: Vec<(u32, KeyModifiers)>,
    sent_down: Vec<u32>,
}

impl BrokerRouter {
    fn modifier(vk: u32) -> bool {
        vk == VK_CONTROL.0 as u32
            || vk == VK_LCONTROL.0 as u32
            || vk == VK_RCONTROL.0 as u32
            || vk == VK_SHIFT.0 as u32
            || vk == VK_LSHIFT.0 as u32
            || vk == VK_RSHIFT.0 as u32
            || is_alt_key(vk)
            || vk == VK_LWIN.0 as u32
            || vk == VK_RWIN.0 as u32
    }

    fn update(&mut self, vk: u32, key_up: bool) {
        let down = !key_up;
        match vk {
            value if value == VK_CONTROL.0 as u32 || value == VK_LCONTROL.0 as u32 => {
                self.ctrl = down
            }
            value if value == VK_RCONTROL.0 as u32 => self.right_ctrl = down,
            value if value == VK_SHIFT.0 as u32 || value == VK_LSHIFT.0 as u32 => self.shift = down,
            value if value == VK_RSHIFT.0 as u32 => self.right_shift = down,
            value if value == VK_LMENU.0 as u32 => self.left_alt = down,
            value if value == VK_RMENU.0 as u32 => self.right_alt = down,
            value if value == VK_LWIN.0 as u32 => self.left_meta = down,
            value if value == VK_RWIN.0 as u32 => self.right_meta = down,
            _ => {}
        }
    }

    fn modifiers(&self) -> KeyModifiers {
        let mut modifiers = KeyModifiers::empty();
        if self.ctrl || self.right_ctrl {
            modifiers |= KeyModifiers::CTRL;
        }
        if self.shift || self.right_shift {
            modifiers |= KeyModifiers::SHIFT;
        }
        if self.left_alt || self.right_alt {
            modifiers |= KeyModifiers::ALT;
        }
        if self.left_meta || self.right_meta {
            modifiers |= KeyModifiers::META;
        }
        modifiers
    }

    fn packet(&self, action: KeyAction, vk: u32) -> InboundPacket {
        InboundPacket::Key {
            action,
            modifiers: self.modifiers(),
            key: vk as u16,
            flags: KeyFlags::empty(),
        }
    }

    fn packet_with_modifiers(action: KeyAction, vk: u32, modifiers: KeyModifiers) -> InboundPacket {
        InboundPacket::Key {
            action,
            modifiers,
            key: vk as u16,
            flags: KeyFlags::empty(),
        }
    }

    fn remember_sent(&mut self, vk: u32) {
        if !self.sent_down.contains(&vk) {
            self.sent_down.push(vk);
        }
    }

    fn forget_sent(&mut self, vk: u32) -> bool {
        if let Some(index) = self.sent_down.iter().position(|key| *key == vk) {
            self.sent_down.remove(index);
            true
        } else {
            false
        }
    }

    fn local_only(&self, vk: u32) -> bool {
        let ctrl = self.ctrl || self.right_ctrl;
        let alt = self.left_alt || self.right_alt;
        let safety_key = vk == 0x2E || vk == VK_Q.0 as u32 || vk == VK_OEM_3.0 as u32;
        let ctrl_alt_local = ctrl && alt && safety_key;
        ctrl_alt_local || (vk == 0x73 && alt)
    }

    fn route(&mut self, vk: u32, key_up: bool) -> (Vec<InboundPacket>, bool, bool) {
        let mut packets = Vec::new();
        let modifier = Self::modifier(vk);
        if !key_up {
            self.update(vk, false);
            if modifier {
                if !self.pending.iter().any(|(key, _)| *key == vk) && !self.sent_down.contains(&vk)
                {
                    self.pending.push((vk, self.modifiers()));
                }
                return (packets, false, false);
            }

            if self.local_only(vk) {
                self.pending.clear();
                return (packets, vk == VK_Q.0 as u32, vk == VK_OEM_3.0 as u32);
            }

            for (pending, modifiers) in std::mem::take(&mut self.pending) {
                packets.push(Self::packet_with_modifiers(
                    KeyAction::Down,
                    pending,
                    modifiers,
                ));
                self.remember_sent(pending);
            }
            packets.push(self.packet(KeyAction::Down, vk));
            self.remember_sent(vk);
            return (packets, false, false);
        }

        if modifier {
            let was_pending = self.pending.iter().position(|(key, _)| *key == vk);
            self.update(vk, true);
            if let Some(index) = was_pending {
                let (pending_vk, down_modifiers) = self.pending.remove(index);
                packets.push(Self::packet_with_modifiers(
                    KeyAction::Down,
                    pending_vk,
                    down_modifiers,
                ));
                packets.push(self.packet(KeyAction::Up, vk));
            } else if self.forget_sent(vk) {
                packets.push(self.packet(KeyAction::Up, vk));
            }
        } else if self.forget_sent(vk) {
            packets.push(self.packet(KeyAction::Up, vk));
        }
        (packets, false, false)
    }

    fn release_sent(&mut self) -> Vec<InboundPacket> {
        let keys = std::mem::take(&mut self.sent_down);
        let mut packets = Vec::with_capacity(keys.len());
        for vk in keys.into_iter().rev() {
            self.update(vk, true);
            packets.push(self.packet(KeyAction::Up, vk));
        }
        self.pending.clear();
        packets
    }
}

static BROKER_STATE: OnceLock<Mutex<Option<BrokerState>>> = OnceLock::new();
const BROKER_INACTIVE: u8 = 0;
const BROKER_STARTING: u8 = 1;
const BROKER_ACTIVE: u8 = 2;
const BROKER_STOPPING: u8 = 3;
static BROKER_PHASE: AtomicU8 = AtomicU8::new(BROKER_INACTIVE);
static BROKER_DISARM_ACKNOWLEDGED: AtomicBool = AtomicBool::new(false);

fn is_broker_active() -> bool {
    BROKER_PHASE.load(Ordering::Acquire) != BROKER_INACTIVE
}

fn broker_state() -> &'static Mutex<Option<BrokerState>> {
    BROKER_STATE.get_or_init(|| Mutex::new(None))
}

struct OwnedWinHandle(HANDLE);

impl Drop for OwnedWinHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn verify_broker_server(pipe: &std::fs::File) -> std::io::Result<()> {
    let pipe_handle = HANDLE(pipe.as_raw_handle());
    let mut process_id = 0;
    unsafe { GetNamedPipeServerProcessId(pipe_handle, &mut process_id) }
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .map(OwnedWinHandle)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) }
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let token = OwnedWinHandle(token);

    let mut needed = 0;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
    if needed < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(std::io::Error::last_os_error());
    }
    let mut bytes = vec![0u8; needed as usize];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(bytes.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    let user = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<TOKEN_USER>()) };
    if !unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) }.as_bool() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "input broker server is not LocalSystem",
        ));
    }
    Ok(())
}

fn broker_header(kind: u16) -> [u8; BROKER_HEADER_SIZE] {
    let mut header = [0u8; BROKER_HEADER_SIZE];
    header[..4].copy_from_slice(b"BPK1");
    header[4..6].copy_from_slice(&1u16.to_le_bytes());
    header[6..8].copy_from_slice(&kind.to_le_bytes());
    header
}

fn install_broker_capture(capture: Arc<CaptureShared>, sender: InputSender) -> bool {
    if BROKER_PHASE.load(Ordering::Acquire) == BROKER_STOPPING
        && !BROKER_DISARM_ACKNOWLEDGED.load(Ordering::Acquire)
    {
        capture.request_disconnect();
        return true;
    }
    let mut state = broker_state()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if state.is_some() {
        return true;
    }
    BROKER_PHASE.store(BROKER_STARTING, Ordering::Release);
    BROKER_DISARM_ACKNOWLEDGED.store(false, Ordering::Release);

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let ownership_started = Arc::new(AtomicBool::new(false));
    let worker_ownership_started = ownership_started.clone();
    let worker_ownership_observed = ownership_started.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let join = match std::thread::Builder::new()
        .name("kbd-broker-pump".into())
        .spawn(move || {
            let result = run_broker_capture(
                worker_stop.clone(),
                worker_ownership_started,
                sender,
                ready_tx,
            );
            if let Err(error) = result {
                tracing::warn!(err = %error, "input broker stopped");
                if error.kind() == std::io::ErrorKind::Interrupted {
                    capture.request_exit();
                } else if worker_ownership_observed.load(Ordering::Acquire) {
                    capture.request_disconnect();
                }
            }
        }) {
        Ok(join) => join,
        Err(error) => {
            tracing::warn!(err = %error, "input broker pump spawn failed");
            BROKER_PHASE.store(BROKER_INACTIVE, Ordering::Release);
            return false;
        }
    };

    match ready_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(true) => {
            tracing::info!("keyboard capture armed through LocalSystem broker");
            *state = Some(BrokerState {
                stop,
                join: Some(join),
            });
            BROKER_PHASE.store(BROKER_ACTIVE, Ordering::Release);
            true
        }
        Ok(false) | Err(_) => {
            BROKER_PHASE.store(BROKER_STOPPING, Ordering::Release);
            stop.store(true, Ordering::Release);
            let _ = join.join();
            if !ownership_started.load(Ordering::Acquire) {
                BROKER_PHASE.store(BROKER_INACTIVE, Ordering::Release);
                false
            } else if BROKER_DISARM_ACKNOWLEDGED.load(Ordering::Acquire) {
                drain_broker_keyboard_messages();
                BROKER_PHASE.store(BROKER_INACTIVE, Ordering::Release);
                false
            } else {
                // Ownership may still exist in the driver. The worker has
                // requested disconnect; keep wndproc gated until process restart.
                true
            }
        }
    }
}

fn run_broker_capture(
    stop: Arc<AtomicBool>,
    ownership_started: Arc<AtomicBool>,
    sender: InputSender,
    ready: std::sync::mpsc::Sender<bool>,
) -> std::io::Result<()> {
    let mut pipe = match OpenOptions::new().read(true).write(true).open(BROKER_PIPE) {
        Ok(pipe) => pipe,
        Err(error) => {
            let _ = ready.send(false);
            return Err(error);
        }
    };
    verify_broker_server(&pipe)?;
    unsafe {
        SetNamedPipeHandleState(HANDLE(pipe.as_raw_handle()), Some(&PIPE_NOWAIT), None, None)
    }
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    // From this point onward ARM may have reached the broker/driver even when a
    // subsequent pipe operation fails. Falling back to wndproc/LL input would
    // risk two producers, so the worker must force a session disconnect.
    ownership_started.store(true, Ordering::Release);
    pipe.write_all(&broker_header(BROKER_KIND_ARM))?;
    pipe.flush()?;
    let mut ready = Some(ready);

    let mut buffered = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut router = BrokerRouter::default();
    let pump_result = (|| -> std::io::Result<()> {
        while !stop.load(Ordering::Acquire) {
            match pipe.read(&mut chunk) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "input broker disconnected",
                    ));
                }
                Ok(read) => buffered.extend_from_slice(&chunk[..read]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(232) =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => return Err(error),
            }

            while buffered.len() >= BROKER_HEADER_SIZE {
                if &buffered[..4] != b"BPK1" || u16::from_le_bytes([buffered[4], buffered[5]]) != 1
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid input broker header",
                    ));
                }
                let kind = u16::from_le_bytes([buffered[6], buffered[7]]);
                let payload_len =
                    u32::from_le_bytes(buffered[8..12].try_into().expect("fixed header")) as usize;
                if payload_len > BROKER_MAX_PAYLOAD {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "input broker payload too large",
                    ));
                }
                let message_len = BROKER_HEADER_SIZE + payload_len;
                if buffered.len() < message_len {
                    break;
                }
                let payload = &buffered[BROKER_HEADER_SIZE..message_len];
                if kind == BROKER_KIND_STATUS {
                    match payload {
                        [1] => {
                            if let Some(ready) = ready.take() {
                                let _ = ready.send(true);
                            }
                        }
                        [0] => {}
                        _ => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "malformed input broker status",
                            ));
                        }
                    }
                } else if kind == BROKER_KIND_EVENTS {
                    forward_broker_events(payload, &sender, &mut router)?;
                } else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unexpected input broker message",
                    ));
                }
                buffered.drain(..message_len);
            }
        }
        Ok(())
    })();
    fn request_broker_disarm(
        pipe: &mut std::fs::File,
        buffered: &mut Vec<u8>,
    ) -> std::io::Result<()> {
        pipe.write_all(&broker_header(BROKER_KIND_DISARM))?;
        pipe.flush()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut chunk = [0u8; 4096];

        loop {
            while buffered.len() >= BROKER_HEADER_SIZE {
                if &buffered[..4] != b"BPK1" || u16::from_le_bytes([buffered[4], buffered[5]]) != 1
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid input broker header during disarm",
                    ));
                }
                let kind = u16::from_le_bytes([buffered[6], buffered[7]]);
                let payload_len =
                    u32::from_le_bytes(buffered[8..12].try_into().expect("fixed header")) as usize;
                if payload_len > BROKER_MAX_PAYLOAD {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "input broker disarm payload too large",
                    ));
                }
                let message_len = BROKER_HEADER_SIZE + payload_len;
                if buffered.len() < message_len {
                    break;
                }
                let disarmed =
                    kind == BROKER_KIND_STATUS && buffered[BROKER_HEADER_SIZE..message_len] == [0];
                let allowed = kind == BROKER_KIND_EVENTS
                    || (kind == BROKER_KIND_STATUS
                        && matches!(&buffered[BROKER_HEADER_SIZE..message_len], [0] | [1]));
                buffered.drain(..message_len);
                if !allowed {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unexpected input broker message during disarm",
                    ));
                }
                if disarmed {
                    return Ok(());
                }
            }

            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "input broker did not acknowledge disarm",
                ));
            }
            match pipe.read(&mut chunk) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "input broker disconnected during disarm",
                    ));
                }
                Ok(read) => buffered.extend_from_slice(&chunk[..read]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(232) =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
    }

    let disarm_result = request_broker_disarm(&mut pipe, &mut buffered);
    if disarm_result.is_ok() {
        BROKER_DISARM_ACKNOWLEDGED.store(true, Ordering::Release);
    }
    let releases = router.release_sent();
    let release_result = if releases.is_empty() || sender.send_batch(&releases) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "session input queue lacks room for broker key releases",
        ))
    };

    disarm_result?;
    release_result?;
    pump_result
}

fn broker_vk(make_code: u16, driver_flags: u16) -> Option<u32> {
    let scan_code = u32::from(make_code)
        | if driver_flags & DRIVER_KEY_E0 != 0 {
            0xE000
        } else if driver_flags & DRIVER_KEY_E1 != 0 {
            0xE100
        } else {
            0
        };
    let vk = unsafe { MapVirtualKeyW(scan_code, MAPVK_VSC_TO_VK_EX) };
    (vk != 0).then_some(vk)
}

fn forward_broker_events(
    payload: &[u8],
    sender: &InputSender,
    router: &mut BrokerRouter,
) -> std::io::Result<()> {
    if payload.len() < 8 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "truncated broker event batch",
        ));
    }
    let count = u32::from_le_bytes(payload[..4].try_into().expect("batch prefix")) as usize;
    let dropped = u32::from_le_bytes(payload[4..8].try_into().expect("batch prefix"));
    if dropped != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "input broker reported dropped events",
        ));
    }
    let expected = 8usize
        .checked_add(count.checked_mul(BROKER_EVENT_SIZE).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "broker event count overflow",
            )
        })?)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "broker batch size overflow",
            )
        })?;
    if payload.len() != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed broker event batch",
        ));
    }

    for event in payload[8..].chunks_exact(BROKER_EVENT_SIZE) {
        if u16::from_le_bytes([event[0], event[1]]) != 1
            || u16::from_le_bytes([event[2], event[3]]) != 1
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unsupported input broker event",
            ));
        }
        let make_code = u16::from_le_bytes([event[16], event[17]]);
        let driver_flags = u16::from_le_bytes([event[18], event[19]]);
        let key_up = driver_flags & DRIVER_KEY_BREAK != 0;
        let Some(vk) = broker_vk(make_code, driver_flags) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unmappable broker scan code",
            ));
        };
        let (packets, exit, disconnect) = router.route(vk, key_up);
        if exit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "broker received local Ctrl+Alt+Q",
            ));
        }
        if disconnect {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "broker received local Ctrl+Alt+backtick",
            ));
        }
        if !packets.is_empty() && !sender.send_batch(&packets) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session input queue lacks room for complete broker transaction",
            ));
        }
    }
    Ok(())
}

fn drain_broker_keyboard_messages() {
    // DISARM has fenced every armed driver callback. Pull all legacy keyboard
    // messages already pending on this UI thread before restoring wndproc as a
    // producer; dropping transition input is safer than forwarding it twice.
    let mut message = MSG::default();
    unsafe { while PeekMessageW(&mut message, None, WM_KEYDOWN, 0x0109, PM_REMOVE).as_bool() {} }
}

fn uninstall_broker_capture() {
    let previous_phase = BROKER_PHASE.swap(BROKER_STOPPING, Ordering::AcqRel);
    let state = broker_state()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let had_state = state.is_some();
    if let Some(mut state) = state {
        state.stop.store(true, Ordering::Release);
        if let Some(join) = state.join.take() {
            let _ = join.join();
        }
    }
    if BROKER_DISARM_ACKNOWLEDGED.load(Ordering::Acquire) {
        drain_broker_keyboard_messages();
        BROKER_PHASE.store(BROKER_INACTIVE, Ordering::Release);
    } else if !had_state && previous_phase != BROKER_STOPPING {
        // No broker epoch ever owned input; this was the LL-hook fallback.
        BROKER_PHASE.store(BROKER_INACTIVE, Ordering::Release);
    }
}
/// Per-session state the hook proc needs — stashed in [`HOOK_STATE`]
/// because a raw `HOOKPROC` gets no user context (no lparam/closure
/// capture, unlike `GWLP_USERDATA` for the wndproc).
#[derive(Clone)]
struct HookShared {
    capture: Arc<CaptureShared>,
    sender: InputSender,
    /// Alt is swallowed before Windows updates its async keyboard state, so
    /// subsequent Tab events must use hook-owned state.
    alt_pressed: Arc<AtomicBool>,
    /// Remembers a swallowed Tab-down until its matching up edge even if the
    /// user releases Alt first.
    alt_tab_active: Arc<AtomicBool>,
}

struct HookState {
    /// Dedicated pump-thread id — `uninstall` posts `WM_QUIT` here.
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
    shared: HookShared,
}

/// Install/uninstall slot for the low-level keyboard hook. `None` when
/// not installed.
static HOOK_STATE: OnceLock<Mutex<Option<HookState>>> = OnceLock::new();

fn hook_state() -> &'static Mutex<Option<HookState>> {
    HOOK_STATE.get_or_init(|| Mutex::new(None))
}

/// Installs the WH_KEYBOARD_LL hook on a DEDICATED message-pump thread
/// (idempotent — a second call while installed is a no-op). A low-level
/// hook executes in the context of its installing thread; installing on
/// the busy eframe/render thread starves callback deadlines under load
/// and Windows then SILENTLY removes the hook (LowLevelHooksTimeout) —
/// field report 2026-07-16: Alt+Tab stopped being swallowed during
/// immersive while the wndproc fallbacks kept working. The dedicated
/// thread does nothing but pump, so hook callbacks return immediately.
pub fn install_keyboard_hook(capture: Arc<CaptureShared>, sender: InputSender) {
    reset_ctrl_tab_remap();
    if !install_broker_capture(capture.clone(), sender.clone()) {
        install_ll_keyboard_hook(capture, sender);
    }
}

fn install_ll_keyboard_hook(capture: Arc<CaptureShared>, sender: InputSender) {
    reset_ctrl_tab_remap();
    let mut guard = hook_state().lock().unwrap_or_else(PoisonError::into_inner);
    if guard.is_some() {
        return;
    }
    let shared = HookShared {
        capture,
        sender,
        alt_pressed: Arc::new(AtomicBool::new(false)),
        alt_tab_active: Arc::new(AtomicBool::new(false)),
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let join = std::thread::Builder::new()
        .name("kb-hook-pump".into())
        .spawn(move || unsafe {
            // Force-create this thread's message queue so the WM_QUIT that
            // uninstall posts can never race a queue that does not exist.
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) {
                Ok(first_hook) => {
                    // LL hook callbacks race Windows' LowLevelHooksTimeout:
                    // exceed it once (scheduling starvation under game/render
                    // load counts) and the hook is silently removed. Time-critical
                    // priority is the standard mitigation for this dedicated pump.
                    {
                        use windows::Win32::System::Threading::{
                            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
                        };
                        if SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)
                            .is_err()
                        {
                            tracing::warn!("kb-hook pump: SetThreadPriority failed");
                        }
                    }
                    let _ = ready_tx.send(Ok(GetCurrentThreadId()));
                    tracing::info!("WH_KEYBOARD_LL installed on dedicated pump thread");
                    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    let _ = UnhookWindowsHookEx(first_hook);
                    tracing::info!("WH_KEYBOARD_LL uninstalled");
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                }
            }
        });
    let join = match join {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(err = %e, "keyboard hook pump thread spawn failed — Win/Alt-Tab capture unavailable");
            return;
        }
    };
    match ready_rx.recv() {
        Ok(Ok(thread_id)) => {
            *guard = Some(HookState {
                thread_id,
                join: Some(join),
                shared,
            });
        }
        Ok(Err(e)) => {
            tracing::warn!(err = %e, "SetWindowsHookExW(WH_KEYBOARD_LL) failed — Win/Alt-Tab capture unavailable");
            let _ = join.join();
        }
        Err(_) => {
            tracing::warn!("keyboard hook pump thread died during install");
            let _ = join.join();
        }
    }
}

/// Uninstalls the hook (idempotent — safe to call with none installed).
pub fn uninstall_keyboard_hook() {
    uninstall_broker_capture();
    reset_ctrl_tab_remap();
    // Take the state and DROP the lock before joining: the hook proc locks
    // the same mutex, so a key event delivered between take() and WM_QUIT
    // would deadlock the pump thread against this join otherwise.
    let state = {
        let mut guard = hook_state().lock().unwrap_or_else(PoisonError::into_inner);
        guard.take()
    };
    if let Some(mut state) = state {
        if !release_sticky_keys(&state.shared.sender) {
            state.shared.capture.request_disconnect();
        }
        unsafe {
            let _ = PostThreadMessageW(state.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = state.join.take() {
            let _ = join.join();
        }
    }
}

/// `WH_KEYBOARD_LL` hook procedure. No user context is available (see
/// [`HookShared`]); `code < 0` must always fall through to
/// `CallNextHookEx` unexamined (SDK contract).
unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let shared = hook_state()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|s| s.shared.clone());
        if let Some(shared) = shared {
            let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let vk = kb.vkCode;
            let msg = wparam.0 as u32;
            let key_up = matches!(msg, WM_KEYUP | WM_SYSKEYUP);
            let ctrl_down = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
            let shift_down = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
            let previous_alt = shared.alt_pressed.load(Ordering::Acquire);
            let alt_pressed = tracked_alt_state(vk, key_up, previous_alt);
            if is_alt_key(vk) {
                shared.alt_pressed.store(alt_pressed, Ordering::Release);
            }
            let alt_tab_active = if vk == VK_TAB.0 as u32 {
                if !key_up {
                    let active = alt_pressed || kb.flags.contains(LLKHF_ALTDOWN);
                    shared.alt_tab_active.store(active, Ordering::Release);
                    active
                } else {
                    alt_pressed || shared.alt_tab_active.swap(false, Ordering::AcqRel)
                }
            } else {
                false
            };
            let alt_down = if vk == VK_TAB.0 as u32 {
                alt_tab_active
            } else if vk == VK_OEM_3.0 as u32 {
                // Ctrl+Alt+` hard disconnect: Ctrl+Alt, shift-agnostic.
                alt_pressed && ctrl_down
            } else {
                // VK_Q escape hatch: Ctrl+Alt, Shift optional.
                alt_pressed && ctrl_down
            };
            // Installation itself is the immersive-capture lifetime boundary.
            // Do not re-gate on the shell atomic here: a stale false edge would
            // pass Alt+Tab to the client OS even though the hook is live.
            match hook_decision(vk, alt_down, key_up, true) {
                HookAction::Pass => {}
                HookAction::ExitImmersive => {
                    shared.capture.request_exit();
                    reset_ctrl_tab_remap();
                    if !release_sticky_keys(&shared.sender) {
                        shared.capture.request_disconnect();
                    }
                    return LRESULT(1);
                }
                HookAction::Disconnect => {
                    release_sticky_keys(&shared.sender);
                    reset_ctrl_tab_remap();
                    shared.capture.request_disconnect();
                    return LRESULT(1);
                }
                HookAction::SwallowForward => {
                    if vk == VK_TAB.0 as u32 && !key_up {
                        tracing::info!("immersive remote Alt+Tab forwarded");
                    }
                    let queued = shared.sender.send(&InboundPacket::Key {
                        action: if key_up {
                            KeyAction::Up
                        } else {
                            KeyAction::Down
                        },
                        modifiers: {
                            // The hook owns Alt state because swallowed Alt
                            // events never update Windows' async key state.
                            // This keeps the complete remote chord ordered and
                            // ensures Tab carries the ALT modifier.
                            let mut m = KeyModifiers::empty();
                            if shift_down {
                                m |= KeyModifiers::SHIFT;
                            }
                            if ctrl_down {
                                m |= KeyModifiers::CTRL;
                            }
                            if !is_alt_key(vk) && (alt_pressed || kb.flags.contains(LLKHF_ALTDOWN))
                            {
                                m |= KeyModifiers::ALT;
                            }
                            if unsafe { GetAsyncKeyState(VK_LWIN.0 as i32) } < 0
                                || unsafe { GetAsyncKeyState(VK_RWIN.0 as i32) } < 0
                            {
                                m |= KeyModifiers::META;
                            }
                            m
                        },
                        key: vk as u16,
                        flags: KeyFlags::empty(),
                    });
                    if !queued {
                        tracing::warn!(
                            "low-level hook input queue saturated — disconnecting to avoid a lost owned edge"
                        );
                        shared.alt_pressed.store(false, Ordering::Release);
                        shared.alt_tab_active.store(false, Ordering::Release);
                        release_sticky_keys(&shared.sender);
                        reset_ctrl_tab_remap();
                        shared.capture.request_disconnect();
                    }
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
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

    #[test]
    fn capture_shared_exit_request_roundtrip() {
        let c = CaptureShared::default();
        assert!(!c.take_exit_requested());
        c.request_exit();
        assert!(c.take_exit_requested());
        // Consuming clears it — a second read this tick sees nothing new.
        assert!(!c.take_exit_requested());
    }

    #[test]
    fn hook_decision_capture_off_always_passes() {
        use HookAction::Pass;
        assert_eq!(hook_decision(VK_LWIN.0 as u32, false, false, false), Pass);
        assert_eq!(hook_decision(VK_TAB.0 as u32, true, false, false), Pass);
        assert_eq!(hook_decision(VK_Q.0 as u32, true, false, false), Pass);
    }

    #[test]
    fn hook_decision_win_keys_swallow_both_edges() {
        use HookAction::SwallowForward;
        assert_eq!(
            hook_decision(VK_LWIN.0 as u32, false, false, true),
            SwallowForward
        );
        assert_eq!(
            hook_decision(VK_RWIN.0 as u32, false, true, true),
            SwallowForward
        );
    }

    #[test]
    fn hook_decision_alt_and_alt_tab_use_the_remote_only_path() {
        use HookAction::{Pass, SwallowForward};
        assert_eq!(
            hook_decision(VK_MENU.0 as u32, false, false, true),
            SwallowForward
        );
        assert_eq!(
            hook_decision(VK_LMENU.0 as u32, false, true, true),
            SwallowForward
        );
        assert_eq!(
            hook_decision(VK_RMENU.0 as u32, false, false, true),
            SwallowForward
        );
        assert_eq!(
            hook_decision(VK_TAB.0 as u32, true, false, true),
            SwallowForward
        );
        assert_eq!(
            hook_decision(VK_TAB.0 as u32, true, true, true),
            SwallowForward
        );
        assert_eq!(hook_decision(VK_TAB.0 as u32, false, false, true), Pass);
    }

    #[test]
    fn swallowed_alt_state_survives_until_its_hook_key_up() {
        assert!(tracked_alt_state(VK_LMENU.0 as u32, false, false));
        assert!(tracked_alt_state(VK_TAB.0 as u32, false, true));
        assert!(!tracked_alt_state(VK_LMENU.0 as u32, true, true));
    }

    #[test]
    fn hook_decision_exit_combo_fires_on_key_down_only() {
        assert_eq!(
            hook_decision(VK_Q.0 as u32, true, false, true),
            HookAction::ExitImmersive
        );
        // Key-up edge and an unsatisfied combo both just pass through.
        assert_eq!(
            hook_decision(VK_Q.0 as u32, true, true, true),
            HookAction::Pass
        );
        assert_eq!(
            hook_decision(VK_Q.0 as u32, false, false, true),
            HookAction::Pass
        );
    }

    #[test]
    fn ctrl_tab_remap_forwards_forward_and_reverse_chords() {
        use KeyAction::Up;

        let mut forward = CtrlTabRemap::default();
        assert_eq!(
            forward.handle(true, WM_KEYDOWN, VK_TAB.0, true, false, false),
            CtrlTabRemapAction::Consume(
                [(Up, VK_CONTROL.0, KeyModifiers::empty())]
                    .into_iter()
                    .chain(alt_tab_chord(false))
                    .collect()
            )
        );

        let mut reverse = CtrlTabRemap::default();
        assert_eq!(
            reverse.handle(true, WM_SYSKEYDOWN, VK_TAB.0, true, false, true),
            CtrlTabRemapAction::Consume(
                [(Up, VK_CONTROL.0, KeyModifiers::empty())]
                    .into_iter()
                    .chain(alt_tab_chord(true))
                    .collect()
            )
        );
    }
    #[test]
    fn alt_tab_chord_orders_modifier_before_tab_and_release_after_tab() {
        assert_eq!(
            alt_tab_chord(false),
            vec![
                (KeyAction::Down, VK_MENU.0, KeyModifiers::ALT),
                (KeyAction::Down, VK_TAB.0, KeyModifiers::ALT),
                (KeyAction::Up, VK_TAB.0, KeyModifiers::ALT),
                (KeyAction::Up, VK_MENU.0, KeyModifiers::empty()),
            ]
        );
    }

    #[test]
    fn ctrl_tab_remap_repeats_without_lifting_ctrl_twice() {
        let mut remap = CtrlTabRemap::default();
        let _ = remap.handle(true, WM_KEYDOWN, VK_TAB.0, true, false, false);

        assert_eq!(
            remap.handle(true, WM_KEYDOWN, VK_TAB.0, true, false, false),
            CtrlTabRemapAction::Consume(alt_tab_chord(false))
        );
    }

    #[test]
    fn ctrl_tab_remap_consumes_the_matched_tab_release_and_restores_ctrl() {
        let mut remap = CtrlTabRemap::default();
        let _ = remap.handle(true, WM_KEYDOWN, VK_TAB.0, true, false, false);

        assert_eq!(
            remap.handle(true, WM_KEYUP, VK_TAB.0, true, false, false),
            CtrlTabRemapAction::Consume(vec![(KeyAction::Down, VK_CONTROL.0, KeyModifiers::CTRL,)])
        );
        assert_eq!(
            remap.handle(true, WM_KEYUP, VK_TAB.0, true, false, false),
            CtrlTabRemapAction::Pass
        );
    }

    #[test]
    fn ctrl_tab_remap_preserves_a_physically_held_shift() {
        let mut remap = CtrlTabRemap::default();
        let CtrlTabRemapAction::Consume(packets) =
            remap.handle(true, WM_KEYDOWN, VK_TAB.0, true, false, true)
        else {
            panic!("Ctrl+Shift+Tab must be remapped");
        };

        assert!(packets.iter().all(|(_, key, _)| *key != VK_SHIFT.0));
        assert_eq!(
            packets,
            [(KeyAction::Up, VK_CONTROL.0, KeyModifiers::empty())]
                .into_iter()
                .chain(alt_tab_chord(true))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn hook_decision_ordinary_keys_pass_through() {
        // 'A' — the wndproc WM_KEYDOWN path already forwards it.
        assert_eq!(hook_decision(0x41, false, false, true), HookAction::Pass);
    }

    #[test]
    fn wndproc_escape_accepts_ctrl_alt_q_with_optional_shift() {
        assert!(is_wndproc_escape(
            true, WM_KEYDOWN, VK_Q.0, true, true, false
        ));
        assert!(is_wndproc_escape(
            true,
            WM_SYSKEYDOWN,
            VK_Q.0,
            true,
            true,
            true
        ));
        assert!(!is_wndproc_escape(
            false, WM_KEYDOWN, VK_Q.0, true, true, false
        ));
        assert!(!is_wndproc_escape(
            true, WM_KEYDOWN, VK_Q.0, true, false, false
        ));
        assert!(!is_wndproc_escape(
            true, WM_KEYUP, VK_Q.0, true, true, false
        ));
    }

    #[test]
    fn disconnect_combo_fires_on_ctrl_alt_backtick_keydown() {
        assert!(is_disconnect_combo(WM_KEYDOWN, VK_OEM_3.0, true, true));
        // Alt held makes it a syskey — same result.
        assert!(is_disconnect_combo(WM_SYSKEYDOWN, VK_OEM_3.0, true, true));
        // Key-up is not a trigger edge.
        assert!(!is_disconnect_combo(WM_KEYUP, VK_OEM_3.0, true, true));
        // Missing either modifier → no disconnect.
        assert!(!is_disconnect_combo(WM_KEYDOWN, VK_OEM_3.0, false, true));
        assert!(!is_disconnect_combo(WM_KEYDOWN, VK_OEM_3.0, true, false));
        // Wrong key → no disconnect.
        assert!(!is_disconnect_combo(WM_KEYDOWN, 0x41, true, true));
    }

    #[test]
    fn hook_decision_disconnect_fires_on_key_down_only() {
        assert_eq!(
            hook_decision(VK_OEM_3.0 as u32, true, false, true),
            HookAction::Disconnect
        );
        // Key-up edge, unsatisfied combo, and capture-off all pass.
        assert_eq!(
            hook_decision(VK_OEM_3.0 as u32, true, true, true),
            HookAction::Pass
        );
        assert_eq!(
            hook_decision(VK_OEM_3.0 as u32, false, false, true),
            HookAction::Pass
        );
        assert_eq!(
            hook_decision(VK_OEM_3.0 as u32, true, false, false),
            HookAction::Pass
        );
    }

    #[test]
    fn capture_shared_disconnect_request_roundtrip() {
        let c = CaptureShared::default();
        assert!(!c.take_disconnect_requested());
        c.request_disconnect();
        assert!(c.take_disconnect_requested());
        // Consuming clears it.
        assert!(!c.take_disconnect_requested());
    }
    #[test]
    fn broker_control_headers_match_protocol_v1() {
        let arm = broker_header(BROKER_KIND_ARM);
        assert_eq!(&arm[..4], b"BPK1");
        assert_eq!(u16::from_le_bytes([arm[4], arm[5]]), 1);
        assert_eq!(u16::from_le_bytes([arm[6], arm[7]]), BROKER_KIND_ARM);
        assert_eq!(
            u32::from_le_bytes(arm[8..12].try_into().expect("length")),
            0
        );
    }

    #[test]
    fn broker_scan_codes_map_alt_tab_and_extended_win() {
        assert!(is_alt_key(broker_vk(0x38, 0).expect("left Alt maps")));
        assert_eq!(broker_vk(0x0f, 0), Some(VK_TAB.0 as u32));
        assert_eq!(broker_vk(0x5b, DRIVER_KEY_E0), Some(VK_LWIN.0 as u32));
    }

    #[test]
    fn broker_router_buffers_and_tracks_local_sequences() {
        let mut router = BrokerRouter::default();

        assert!(router.route(VK_LMENU.0 as u32, false).0.is_empty());
        assert_eq!(router.route(VK_TAB.0 as u32, false).0.len(), 2); // Alt+Tab
        assert_eq!(router.route(VK_TAB.0 as u32, true).0.len(), 1);
        assert_eq!(router.route(VK_LMENU.0 as u32, true).0.len(), 1);

        assert!(router.route(VK_LWIN.0 as u32, false).0.is_empty());
        assert_eq!(router.route(0x52, false).0.len(), 2); // Win+R
        assert_eq!(router.route(0x52, true).0.len(), 1);
        assert_eq!(router.route(VK_LWIN.0 as u32, true).0.len(), 1);

        assert_eq!(router.route(0x41, false).0.len(), 1); // ordinary A
        assert_eq!(router.route(0x41, true).0.len(), 1);

        assert!(router.route(VK_CONTROL.0 as u32, false).0.is_empty());
        assert!(router.route(VK_LMENU.0 as u32, false).0.is_empty());
        assert!(router.route(0x2E, false).0.is_empty()); // Ctrl+Alt+Delete
        assert!(router.route(VK_LMENU.0 as u32, true).0.is_empty());
        assert!(router.route(VK_CONTROL.0 as u32, true).0.is_empty());

        assert!(router.route(VK_SHIFT.0 as u32, false).0.is_empty());
        let shift_tap = router.route(VK_SHIFT.0 as u32, true).0;
        assert_eq!(shift_tap.len(), 2);
        assert!(matches!(
            &shift_tap[..],
            [
                InboundPacket::Key {
                    action: KeyAction::Down,
                    key,
                    ..
                },
                InboundPacket::Key {
                    action: KeyAction::Up,
                    key: up_key,
                    ..
                }
            ] if *key == VK_SHIFT.0 && *up_key == VK_SHIFT.0
        ));
        assert!(router.release_sent().is_empty());
        assert!(router.route(VK_LMENU.0 as u32, false).0.is_empty());
        assert!(router.route(VK_SHIFT.0 as u32, false).0.is_empty());
        assert_eq!(router.route(VK_TAB.0 as u32, false).0.len(), 3); // Alt+Shift+Tab
        assert_eq!(router.route(VK_TAB.0 as u32, true).0.len(), 1);
        assert_eq!(router.route(VK_SHIFT.0 as u32, true).0.len(), 1);
        assert_eq!(router.route(VK_LMENU.0 as u32, true).0.len(), 1);

        assert!(router.route(VK_LMENU.0 as u32, false).0.is_empty());
        assert!(router.route(0x73, false).0.is_empty()); // local Alt+F4
        assert!(router.route(VK_LMENU.0 as u32, true).0.is_empty());

        assert!(router.route(VK_CONTROL.0 as u32, false).0.is_empty());
        assert!(router.route(VK_LMENU.0 as u32, false).0.is_empty());
        assert!(router.route(VK_Q.0 as u32, false).1); // local exit
        assert!(router.route(VK_LMENU.0 as u32, true).0.is_empty());
        assert!(router.route(VK_CONTROL.0 as u32, true).0.is_empty());

        assert!(router.route(VK_CONTROL.0 as u32, false).0.is_empty());
        assert!(router.route(VK_LMENU.0 as u32, false).0.is_empty());
        assert!(router.route(VK_OEM_3.0 as u32, false).2); // local disconnect
        assert!(router.route(VK_LMENU.0 as u32, true).0.is_empty());
        assert!(router.route(VK_CONTROL.0 as u32, true).0.is_empty());
    }

    #[test]
    fn broker_router_forwards_standalone_alt_and_win_taps() {
        for (vk, expected_down_modifier) in [
            (VK_LMENU.0 as u32, KeyModifiers::ALT),
            (VK_LWIN.0 as u32, KeyModifiers::META),
        ] {
            let mut router = BrokerRouter::default();
            assert!(router.route(vk, false).0.is_empty());

            let packets = router.route(vk, true).0;
            assert!(matches!(
                &packets[..],
                [
                    InboundPacket::Key {
                        action: KeyAction::Down,
                        modifiers: down_modifiers,
                        key: down_key,
                        ..
                    },
                    InboundPacket::Key {
                        action: KeyAction::Up,
                        modifiers: up_modifiers,
                        key: up_key,
                        ..
                    }
                ] if *down_key == vk as u16
                    && *up_key == vk as u16
                    && *down_modifiers == expected_down_modifier
                    && up_modifiers.is_empty()
            ));
            assert!(router.release_sent().is_empty());
        }
    }
    #[test]
    fn broker_router_tracks_ordered_modifier_edges() {
        let mut state = BrokerRouter::default();
        state.update(VK_LMENU.0 as u32, false);
        state.update(VK_RMENU.0 as u32, false);
        assert!(state.left_alt || state.right_alt);
        state.update(VK_LMENU.0 as u32, true);
        assert!(state.right_alt);
        state.update(VK_RMENU.0 as u32, true);
        assert!(!state.left_alt && !state.right_alt);

        state.update(VK_LWIN.0 as u32, false);
        state.update(VK_RWIN.0 as u32, false);
        state.update(VK_LWIN.0 as u32, true);
        assert!(state.right_meta);
        state.update(VK_RWIN.0 as u32, true);
        assert!(!state.left_meta && !state.right_meta);
    }
}
