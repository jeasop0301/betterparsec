//! Host-authority cursor visibility tracker — M4 cursor P1
//! (docs/design/cursor-channel.md §3).
//!
//! The streamer runs on the host machine, so no Sunshine fork is needed:
//! a 60 Hz Win32 poll reads `GetCursorInfo` (visibility + position) and
//! publishes POS messages whenever the sample changes. The client's auto
//! mouse-mode consumes them: `visible=false` → pointer lock (FPS aim),
//! `visible=true` → unlock + absolute input, with the in-video baked
//! cursor as the visual (Sunshine keeps blending in P1).
//!
//! Host-cursor authority is transport-agnostic (DCV ships it over TCP),
//! so the tracker writes through a [`CursorSink`]: a dedicated reliable
//! DataChannel on WebRTC, a `TransportChannelId::CURSOR`-prefixed frame
//! on the WebSocket transport.
//!
//! `CURSOR_SUPPRESSED` (touch) counts as hidden per the design. The
//! sample uses `CURSORINFO.ptScreenPos` + `GetSystemMetrics` — one
//! consistent (DPI-virtualized) coordinate space; the client only needs
//! the normalized position, so consistency beats "physical" (deviation
//! from the doc's GetPhysicalCursorPos noted deliberately: mixing spaces
//! is the actual hazard).
//!
//! Lifecycle: spawned once per transport; exits when the sink reports
//! closed after having been ready, or after repeated send failures
//! (peer gone). Non-Windows builds sample nothing and idle.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use common::api_bindings::TransportChannelId;
use common::ipc::StreamerIpcMessage;
use tokio::sync::mpsc::Sender;
use tokio::time::MissedTickBehavior;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::TransportEvent;
use super::cursor_wire::{CURSOR_POS_LEN, CursorPos, encode_pos};

/// Consecutive send failures before the task gives up (peer gone).
const MAX_SEND_FAILURES: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkState {
    /// Not open yet — keep polling, do not send.
    Wait,
    /// Open — send on change.
    Ready,
    /// Gone — the tracker exits.
    Closed,
}

/// Transport-specific write half for the cursor channel.
#[async_trait]
pub(crate) trait CursorSink: Send + Sync + 'static {
    fn state(&self) -> SinkState;
    /// `false` = send failure (counted toward the give-up threshold).
    async fn send(&self, pos: CursorPos) -> bool;
}

/// WebRTC: the dedicated reliable+ordered `cursor` DataChannel.
pub(crate) struct WebRtcCursorSink(pub(crate) Arc<RTCDataChannel>);

#[async_trait]
impl CursorSink for WebRtcCursorSink {
    fn state(&self) -> SinkState {
        match self.0.ready_state() {
            RTCDataChannelState::Open => SinkState::Ready,
            RTCDataChannelState::Closing | RTCDataChannelState::Closed => SinkState::Closed,
            _ => SinkState::Wait,
        }
    }

    async fn send(&self, pos: CursorPos) -> bool {
        self.0
            .send(&Bytes::copy_from_slice(&encode_pos(pos)))
            .await
            .is_ok()
    }
}

/// WebSocket: a `TransportChannelId::CURSOR`-prefixed binary frame routed
/// through the server relay (same framing as every other ws channel).
pub(crate) struct WebSocketCursorSink(pub(crate) Sender<TransportEvent>);

#[async_trait]
impl CursorSink for WebSocketCursorSink {
    fn state(&self) -> SinkState {
        if self.0.is_closed() {
            SinkState::Closed
        } else {
            SinkState::Ready
        }
    }

    async fn send(&self, pos: CursorPos) -> bool {
        self.0
            .send(TransportEvent::SendIpc(
                StreamerIpcMessage::WebSocketTransport(Bytes::from(ws_cursor_frame(pos))),
            ))
            .await
            .is_ok()
    }
}

/// `[CURSOR channel id][14-byte POS]` — the WebSocket wire frame.
pub(crate) fn ws_cursor_frame(pos: CursorPos) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + CURSOR_POS_LEN);
    out.push(TransportChannelId::CURSOR);
    out.extend_from_slice(&encode_pos(pos));
    out
}

pub(crate) fn spawn(sink: impl CursorSink) {
    tokio::spawn(run(sink));
}

async fn run(sink: impl CursorSink) {
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut last: Option<CursorPos> = None;
    let mut failures = 0u32;

    loop {
        tick.tick().await;

        match sink.state() {
            SinkState::Ready => {}
            SinkState::Closed => {
                tracing::debug!("[Cursor] sink closed — tracker exiting");
                return;
            }
            SinkState::Wait => continue,
        }

        let Some(cur) = sample() else { continue };
        if last == Some(cur) {
            continue;
        }
        // Send on any change (visibility flip or movement). Reliable and
        // ordered: state transitions must not be lost, and 60 Hz × 14 B is
        // negligible retransmission load.
        if sink.send(cur).await {
            failures = 0;
            last = Some(cur);
        } else {
            failures += 1;
            if failures >= MAX_SEND_FAILURES {
                tracing::debug!("[Cursor] repeated send failures — tracker exiting");
                return;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The ws frame is the POS wire behind the CURSOR channel id — pinned
    /// against the same vector as the cursor_wire byte pin.
    #[test]
    fn ws_frame_prefixes_channel_id() {
        let frame = ws_cursor_frame(CursorPos {
            visible: true,
            x: 1000,
            y: -2,
            vw: 2560,
            vh: 1440,
        });
        assert_eq!(frame.len(), 1 + CURSOR_POS_LEN);
        assert_eq!(frame[0], TransportChannelId::CURSOR);
        assert_eq!(frame[0], 27);
        assert_eq!(
            &frame[1..],
            &[
                0x00, 0x01, 0xE8, 0x03, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0x0A, 0xA0, 0x05
            ]
        );
    }
}
