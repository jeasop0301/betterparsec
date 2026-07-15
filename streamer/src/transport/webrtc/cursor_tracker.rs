//! Host-authority cursor visibility tracker — M4 cursor P1
//! (docs/design/cursor-channel.md §3).
//!
//! The streamer runs on the host machine, so no Sunshine fork is needed:
//! a 60 Hz Win32 poll reads `GetCursorInfo` (visibility + position) and
//! publishes POS messages on the reliable, ordered `cursor` DataChannel
//! whenever the sample changes. The client's auto mouse-mode consumes
//! them: `visible=false` → pointer lock (FPS aim), `visible=true` →
//! unlock + absolute input, with the in-video baked cursor as the visual
//! (Sunshine keeps blending in P1).
//!
//! `CURSOR_SUPPRESSED` (touch) counts as hidden per the design. The
//! sample uses `CURSORINFO.ptScreenPos` + `GetSystemMetrics` — one
//! consistent (DPI-virtualized) coordinate space; the client only needs
//! the normalized position, so consistency beats "physical" (deviation
//! from the doc's GetPhysicalCursorPos noted deliberately: mixing spaces
//! is the actual hazard).
//!
//! Lifecycle: spawned once per transport; exits when the channel leaves
//! the open state after having opened, or after repeated send failures
//! (peer gone). Non-Windows builds sample nothing and idle.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::cursor_wire::{CursorPos, encode_pos};

/// Consecutive send failures before the task gives up (peer gone).
const MAX_SEND_FAILURES: u32 = 10;

pub(crate) fn spawn(channel: Arc<RTCDataChannel>) {
    tokio::spawn(run(channel));
}

async fn run(channel: Arc<RTCDataChannel>) {
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut last: Option<CursorPos> = None;
    let mut failures = 0u32;

    loop {
        tick.tick().await;

        match channel.ready_state() {
            RTCDataChannelState::Open => {}
            RTCDataChannelState::Closing | RTCDataChannelState::Closed => {
                tracing::debug!("[Cursor] channel closed — tracker exiting");
                return;
            }
            // Connecting/unspecified: wait for open.
            _ => continue,
        }

        let Some(cur) = sample() else { continue };
        if last == Some(cur) {
            continue;
        }
        // Send on any change (visibility flip or movement). Reliable and
        // ordered: state transitions must not be lost, and 60 Hz × 14 B is
        // negligible retransmission load.
        if channel
            .send(&bytes::Bytes::copy_from_slice(&encode_pos(cur)))
            .await
            .is_err()
        {
            failures += 1;
            if failures >= MAX_SEND_FAILURES {
                tracing::debug!("[Cursor] repeated send failures — tracker exiting");
                return;
            }
        } else {
            failures = 0;
            last = Some(cur);
        }
    }
}

#[cfg(windows)]
fn sample() -> Option<CursorPos> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };

    unsafe {
        let mut ci: CURSORINFO = std::mem::zeroed();
        ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
        if GetCursorInfo(&mut ci) == 0 {
            return None;
        }
        // Exactly CURSOR_SHOWING = visible; CURSOR_SUPPRESSED (touch) and
        // 0 (hidden by ShowCursor/DirectInput) are both "hidden".
        let visible = ci.flags == CURSOR_SHOWING;
        let vw = GetSystemMetrics(SM_CXSCREEN);
        let vh = GetSystemMetrics(SM_CYSCREEN);
        if vw <= 0 || vh <= 0 {
            return None;
        }
        Some(CursorPos {
            visible,
            x: ci.ptScreenPos.x,
            y: ci.ptScreenPos.y,
            vw: vw.min(u16::MAX as i32) as u16,
            vh: vh.min(u16::MAX as i32) as u16,
        })
    }
}

#[cfg(not(windows))]
fn sample() -> Option<CursorPos> {
    None
}
