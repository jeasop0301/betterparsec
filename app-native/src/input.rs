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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use client_transport::session::InputSender;
use common::input_wire::InboundPacket;
use moonlight_common::stream::control::{
    KeyAction, KeyFlags, KeyModifiers, MouseButton, MouseButtonAction,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_LWIN,
    VK_MENU, VK_OEM_3, VK_Q, VK_RWIN, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetClientRect, HHOOK, HTCLIENT, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, SetCursor,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_INPUT, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
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
/// when immersive capture tears down. After an exit (the Ctrl+Alt+Shift+Q
/// hatch holds those three; Alt+Tab holds Alt; the Start-menu swallow
/// holds Win) the stream child can lose focus before the physical key-ups
/// arrive, so those key-ups never reach the wire and the host keeps them
/// latched (field report: "Alt stays held"). Releasing them explicitly on
/// every exit clears it; a redundant up for a key that was not down is a
/// no-op on the host.
pub fn release_sticky_keys(sender: &InputSender) {
    for vk in [
        VK_MENU, VK_CONTROL, VK_SHIFT, VK_LWIN, VK_RWIN, VK_TAB, VK_Q,
    ] {
        sender.send(&InboundPacket::Key {
            action: KeyAction::Up,
            modifiers: KeyModifiers::empty(),
            key: vk.0,
            flags: KeyFlags::empty(),
        });
    }
}

/// Pure predicate for the hook-independent immersive escape in [`handle`]:
/// Ctrl+Alt+Shift+Q on a key-down while relative capture is engaged.
/// Mirrors the LL-hook `hook_decision` combo so that a *failed* hook
/// install (`SetWindowsHookExW` only warns) still leaves a way out — the
/// clipped cursor otherwise traps the user with the sidebar button
/// unreachable (field report: had to kill the app). When the hook is
/// installed it swallows Q before the wndproc sees it, so this never
/// double-fires.
fn is_wndproc_escape(
    relative: bool,
    msg: u32,
    vk: u16,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> bool {
    relative && matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && vk == VK_Q.0 && ctrl && alt && shift
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

    // Genuine focus loss while captured (Alt+Tab that slipped past a
    // failed keyboard hook, a system dialog, UAC): the stream child holds
    // keyboard focus during immersive, so WM_KILLFOCUS here means a real
    // loss, not our own re-focus. Release held modifiers (the host would
    // otherwise keep Alt latched) and request immersive exit so the cursor
    // unclips and the user is never trapped fullscreen.
    if msg == WM_KILLFOCUS && ctx.capture.relative() {
        release_sticky_keys(&ctx.sender);
        ctx.capture.request_exit();
        return None; // DefWindowProc still does its normal kill-focus work
    }
    // Hook-independent escape (see is_wndproc_escape): works whenever the
    // captured child has focus, so a failed keyboard-hook install cannot
    // trap the user.
    if is_wndproc_escape(
        ctx.capture.relative(),
        msg,
        wparam.0 as u16,
        unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0,
        unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0,
        unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0,
    ) {
        ctx.capture.request_exit();
        release_sticky_keys(&ctx.sender);
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
        ctx.capture.request_disconnect();
        return Some(LRESULT(0)); // consume ` — never forward it
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
// sidebar "Exit immersive" button is unreachable, and Windows would
// otherwise steal the Win keys and Alt+Tab away from the host (Start
// menu / task switcher) instead of forwarding them — the Keyboard Lock
// analog (web `keyboard.lock()`; see docs/design/unified-app-architecture.md
// §4-3). A WH_KEYBOARD_LL hook intercepts those keys ahead of any window
// getting them and either forwards them onto the wire or fires the
// Ctrl+Alt+Shift+Q escape hatch.

/// Outcome of [`hook_decision`] for one low-level keyboard event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    /// Let Windows (and, after `CallNextHookEx`, the app's own
    /// WM_KEYDOWN/UP path in [`handle`]) process the key normally.
    Pass,
    /// Windows must never see this key: forward it to the host on the
    /// wire and swallow it here (`CallNextHookEx` is skipped).
    SwallowForward,
    /// The Ctrl+Alt+Shift+Q escape hatch fired.
    ExitImmersive,
    /// The Ctrl+Alt+` hard-disconnect hotkey fired (Parsec parity).
    Disconnect,
}

/// Pure Keyboard Lock decision table (unit-tested, no Win32).
///
/// `alt_down` carries whichever extra-modifier condition matters for
/// `vk`: the literal Alt-down flag (`KBDLLHOOKSTRUCT` `LLKHF_ALTDOWN`)
/// for `VK_TAB`, or the full Ctrl+Alt+Shift combo — tracked by the
/// caller from the hook's own key stream or `GetAsyncKeyState`, since
/// `GetKeyState` in a low-level hook can lag the event that is still
/// in-flight — for the `VK_Q` escape hatch. Rules:
/// - `capture_on == false` ⇒ [`HookAction::Pass`] (hook installed but
///   immersive not engaged — e.g. a race during teardown).
/// - `VK_LWIN`/`VK_RWIN` ⇒ [`HookAction::SwallowForward`] (Start menu).
/// - `VK_TAB` with `alt_down` ⇒ [`HookAction::SwallowForward`] (task
///   switcher).
/// - `VK_Q` key-down with `alt_down` (Ctrl+Alt+Shift satisfied) ⇒
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
    let is_win = vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32;
    let is_alt_tab = vk == VK_TAB.0 as u32 && alt_down;
    if is_win || is_alt_tab {
        HookAction::SwallowForward
    } else {
        HookAction::Pass
    }
}

/// An `HHOOK` is a process-global handle, not tied to the installing
/// thread (same reasoning as `cursor_icon::OwnedCursor`); wrap it so the
/// static slot below can hold it.
struct HookHandle(HHOOK);
unsafe impl Send for HookHandle {}

/// Per-session state the hook proc needs — stashed in [`HOOK_STATE`]
/// because a raw `HOOKPROC` gets no user context (no lparam/closure
/// capture, unlike `GWLP_USERDATA` for the wndproc).
#[derive(Clone)]
struct HookShared {
    capture: Arc<CaptureShared>,
    sender: InputSender,
}

struct HookState {
    hook: HookHandle,
    shared: HookShared,
}

/// Install/uninstall slot for the low-level keyboard hook. `None` when
/// not installed.
static HOOK_STATE: OnceLock<Mutex<Option<HookState>>> = OnceLock::new();

fn hook_state() -> &'static Mutex<Option<HookState>> {
    HOOK_STATE.get_or_init(|| Mutex::new(None))
}

/// Installs the WH_KEYBOARD_LL hook (idempotent — a second call while
/// already installed is a no-op). **Must run on the UI thread**: a
/// low-level hook executes in the context of the thread that installed
/// it, and that thread must keep pumping messages (`GetMessage`/
/// `DispatchMessage`) or the hook call adds visible system-wide input
/// lag — the eframe main thread already does this for the window
/// message loop, so it qualifies.
pub fn install_keyboard_hook(capture: Arc<CaptureShared>, sender: InputSender) {
    let mut guard = hook_state().lock().unwrap_or_else(PoisonError::into_inner);
    if guard.is_some() {
        return;
    }
    let shared = HookShared { capture, sender };
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) };
    match hook {
        Ok(h) => {
            *guard = Some(HookState {
                hook: HookHandle(h),
                shared,
            })
        }
        Err(e) => {
            tracing::warn!(err = %e, "SetWindowsHookExW(WH_KEYBOARD_LL) failed — Win/Alt-Tab capture unavailable");
        }
    }
}

/// Uninstalls the hook (idempotent — safe to call with none installed).
pub fn uninstall_keyboard_hook() {
    let mut guard = hook_state().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(state) = guard.take() {
        unsafe {
            let _ = UnhookWindowsHookEx(state.hook.0);
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
            let alt_down = if vk == VK_TAB.0 as u32 {
                kb.flags.contains(LLKHF_ALTDOWN)
            } else if vk == VK_OEM_3.0 as u32 {
                // Ctrl+Alt+` hard disconnect: Ctrl+Alt, shift-agnostic.
                kb.flags.contains(LLKHF_ALTDOWN) && ctrl_down
            } else {
                // VK_Q escape hatch: full Ctrl+Alt+Shift combo.
                kb.flags.contains(LLKHF_ALTDOWN) && ctrl_down && shift_down
            };
            match hook_decision(vk, alt_down, key_up, shared.capture.relative()) {
                HookAction::Pass => {}
                HookAction::ExitImmersive => {
                    shared.capture.request_exit();
                    release_sticky_keys(&shared.sender);
                    return LRESULT(1);
                }
                HookAction::Disconnect => {
                    release_sticky_keys(&shared.sender);
                    shared.capture.request_disconnect();
                    return LRESULT(1);
                }
                HookAction::SwallowForward => {
                    shared.sender.send(&InboundPacket::Key {
                        action: if key_up {
                            KeyAction::Up
                        } else {
                            KeyAction::Down
                        },
                        modifiers: {
                            // Build modifiers from the reliable hook signals:
                            // GetKeyState (used by current_modifiers) can miss
                            // keys in a low-level hook's thread, dropping the
                            // Alt off a forwarded Alt+Tab so the host never sees
                            // the combo (field report: Alt+Tab does nothing).
                            let mut m = KeyModifiers::empty();
                            if shift_down {
                                m |= KeyModifiers::SHIFT;
                            }
                            if ctrl_down {
                                m |= KeyModifiers::CTRL;
                            }
                            if kb.flags.contains(LLKHF_ALTDOWN) {
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
    fn hook_decision_alt_tab_needs_alt_flag() {
        assert_eq!(
            hook_decision(VK_TAB.0 as u32, true, false, true),
            HookAction::SwallowForward
        );
        // Plain Tab (no Alt) is not the task-switcher combo.
        assert_eq!(
            hook_decision(VK_TAB.0 as u32, false, false, true),
            HookAction::Pass
        );
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
    fn hook_decision_ordinary_keys_pass_through() {
        // 'A' — the wndproc WM_KEYDOWN path already forwards it.
        assert_eq!(hook_decision(0x41, false, false, true), HookAction::Pass);
    }

    #[test]
    fn wndproc_escape_fires_only_on_full_combo_keydown_while_captured() {
        // Full Ctrl+Alt+Shift+Q key-down while captured → escape.
        assert!(is_wndproc_escape(
            true, WM_KEYDOWN, VK_Q.0, true, true, true
        ));
        // Alt makes Q a syskey — same result.
        assert!(is_wndproc_escape(
            true,
            WM_SYSKEYDOWN,
            VK_Q.0,
            true,
            true,
            true
        ));
        // Not captured → never (normal desktop use must not trap Q).
        assert!(!is_wndproc_escape(
            false, WM_KEYDOWN, VK_Q.0, true, true, true
        ));
        // Missing any modifier → no escape.
        assert!(!is_wndproc_escape(
            true, WM_KEYDOWN, VK_Q.0, true, false, true
        ));
        // Wrong key → no escape.
        assert!(!is_wndproc_escape(true, WM_KEYDOWN, 0x41, true, true, true));
        // Key-up (WM_KEYUP) is not a trigger edge.
        assert!(!is_wndproc_escape(true, WM_KEYUP, VK_Q.0, true, true, true));
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
}
