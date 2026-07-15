//! Clipboard sync v1 (text-only) — closes a shipped-competitor gap noted in
//! docs/research/05 and tracked in docs/ROADMAP.md ("클립보드 동기화").
//! Parsec and DCV both ship clipboard sync; the streamer runs on the host
//! machine, so — same as the cursor channel — no Sunshine fork is needed.
//!
//! Bidirectional: a 500 ms Win32 poll of `GetClipboardSequenceNumber`
//! detects host-side clipboard changes (Win32 has no cross-process
//! clipboard-change event usable without a hidden message-only window;
//! polling matches the cursor tracker's approach) and publishes CF_UNICODETEXT
//! through a [`ClipboardSink`] — a dedicated reliable DataChannel on WebRTC,
//! a `TransportChannelId::CLIPBOARD`-prefixed frame on the WebSocket
//! transport, mirroring cursor_tracker.rs exactly. Inbound text (client copy
//! -> host paste) arrives via an mpsc `Receiver<String>` that each transport
//! feeds from its decoded CLIPBOARD messages; this task owns writing it to
//! the host clipboard.
//!
//! Loop guard: without it, applying inbound text bumps the clipboard
//! sequence number, the next poll samples it right back, and it echoes to
//! the peer forever. `should_publish` (pure, unit-tested) suppresses
//! publishing a sample that matches the last value this task applied FROM
//! the peer, or the last value it already sent.
//!
//! Wire format: `web/stream/clipboard_wire.ts` mirrors `encode_text` /
//! `decode_text` byte-for-byte — keep the pinned tests in lockstep.
//!
//! Non-Windows builds: the sequence-number poll always reads 0 (idles after
//! the first tick), the Win32 read/write helpers are no-ops, and the apply
//! path drains and drops inbound text instead of writing it anywhere.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use common::api_bindings::TransportChannelId;
use common::ipc::StreamerIpcMessage;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::spawn_blocking;
use tokio::time::MissedTickBehavior;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::TransportEvent;

/// Consecutive send failures before the watcher gives up (peer gone).
const MAX_SEND_FAILURES: u32 = 10;
/// Host clipboard poll period.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub const CLIPBOARD_KIND_TEXT: u8 = 0;
/// 256 KiB — generous for pasted text/URLs/code, small enough that a runaway
/// clipboard (e.g. a whole log file copied by accident) cannot flood either
/// transport.
pub const CLIPBOARD_MAX_LEN: usize = 262_144;

/// `u8 kind=0 (TEXT) | u32 len (LE) | utf8 bytes`.
///
/// No cap enforcement here — the signature returns `Vec<u8>`, not `Option`,
/// so callers that can produce oversized text (the host watcher, inbound
/// apply path) check [`exceeds_cap`] first and skip the send/apply entirely
/// instead of shipping a frame the peer's [`decode_text`] would reject.
pub fn encode_text(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(5 + bytes.len());
    out.push(CLIPBOARD_KIND_TEXT);
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

/// `None` on truncated/malformed input, an oversized declared length, or an
/// unknown `kind` byte — unknown kinds are ignored rather than treated as an
/// error so a future TS/Rust side can add e.g. an IMAGE kind without an
/// immediate hard break on the other side.
pub fn decode_text(data: &[u8]) -> Option<String> {
    if data.len() < 5 || data[0] != CLIPBOARD_KIND_TEXT {
        return None;
    }
    let len = u32::from_le_bytes(data[1..5].try_into().ok()?) as usize;
    if len > CLIPBOARD_MAX_LEN {
        return None;
    }
    let body = data.get(5..5 + len)?;
    String::from_utf8(body.to_vec()).ok()
}

/// Write-side cap guard (read-side enforcement lives in [`decode_text`]).
pub(crate) fn exceeds_cap(text: &str) -> bool {
    text.len() > CLIPBOARD_MAX_LEN
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkState {
    /// Not open yet — keep polling, do not send.
    Wait,
    /// Open — send on change.
    Ready,
    /// Gone — the watcher exits.
    Closed,
}

/// Transport-specific write half for the clipboard channel.
#[async_trait]
pub(crate) trait ClipboardSink: Send + Sync + 'static {
    fn state(&self) -> SinkState;
    /// `false` = send failure (counted toward the give-up threshold).
    async fn send(&self, text: &str) -> bool;
}

/// WebRTC: the dedicated reliable+ordered `clipboard` DataChannel.
pub(crate) struct WebRtcClipboardSink(pub(crate) Arc<RTCDataChannel>);

#[async_trait]
impl ClipboardSink for WebRtcClipboardSink {
    fn state(&self) -> SinkState {
        match self.0.ready_state() {
            RTCDataChannelState::Open => SinkState::Ready,
            RTCDataChannelState::Closing | RTCDataChannelState::Closed => SinkState::Closed,
            _ => SinkState::Wait,
        }
    }

    async fn send(&self, text: &str) -> bool {
        self.0.send(&Bytes::from(encode_text(text))).await.is_ok()
    }
}

/// WebSocket: a `TransportChannelId::CLIPBOARD`-prefixed binary frame routed
/// through the server relay (same framing as every other ws channel).
pub(crate) struct WebSocketClipboardSink(pub(crate) Sender<TransportEvent>);

#[async_trait]
impl ClipboardSink for WebSocketClipboardSink {
    fn state(&self) -> SinkState {
        if self.0.is_closed() {
            SinkState::Closed
        } else {
            SinkState::Ready
        }
    }

    async fn send(&self, text: &str) -> bool {
        self.0
            .send(TransportEvent::SendIpc(
                StreamerIpcMessage::WebSocketTransport(Bytes::from(ws_clipboard_frame(text))),
            ))
            .await
            .is_ok()
    }
}

/// `[CLIPBOARD channel id][wire bytes]` — the WebSocket wire frame.
pub(crate) fn ws_clipboard_frame(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 5 + text.len());
    out.push(TransportChannelId::CLIPBOARD);
    out.extend_from_slice(&encode_text(text));
    out
}

/// Loop-guard/dedupe: should the watcher publish `current` (the host
/// clipboard text just sampled)?
///
/// - `current == last_applied` — this is the echo of the text this task most
///   recently wrote to the host clipboard on behalf of the peer. Suppressing
///   it is what stops an inbound paste from bouncing straight back out.
/// - `current == last_sent` — already published this exact value; the
///   sequence number can change without the text changing (re-copying the
///   same content), so this avoids a redundant send.
pub(crate) fn should_publish(
    current: &str,
    last_applied: Option<&str>,
    last_sent: Option<&str>,
) -> bool {
    if last_applied == Some(current) {
        return false;
    }
    if last_sent == Some(current) {
        return false;
    }
    true
}

pub(crate) fn spawn(sink: impl ClipboardSink, apply_rx: Receiver<String>) {
    tokio::spawn(run(sink, apply_rx));
}

async fn run(sink: impl ClipboardSink, mut apply_rx: Receiver<String>) {
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut last_seq: Option<u32> = None;
    let mut last_sent: Option<String> = None;
    let mut last_applied: Option<String> = None;
    let mut failures = 0u32;
    // The apply_rx branch is disabled once the sender side is dropped so the
    // select! loop does not spin on a permanently-ready `Ready(None)`.
    let mut apply_closed = false;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                match sink.state() {
                    SinkState::Ready => {}
                    SinkState::Closed => {
                        tracing::debug!("[Clipboard] sink closed — watcher exiting");
                        return;
                    }
                    SinkState::Wait => continue,
                }

                let seq = sequence_number();
                if last_seq == Some(seq) {
                    continue;
                }
                last_seq = Some(seq);

                let Some(current) = spawn_blocking(read_clipboard_text).await.unwrap_or(None) else {
                    continue;
                };

                if !should_publish(&current, last_applied.as_deref(), last_sent.as_deref()) {
                    continue;
                }

                if exceeds_cap(&current) {
                    tracing::debug!(
                        "[Clipboard] host clipboard text ({} bytes) exceeds the {CLIPBOARD_MAX_LEN}-byte cap — refusing to send",
                        current.len()
                    );
                    continue;
                }

                if sink.send(&current).await {
                    failures = 0;
                    last_sent = Some(current);
                } else {
                    failures += 1;
                    if failures >= MAX_SEND_FAILURES {
                        tracing::debug!("[Clipboard] repeated send failures — watcher exiting");
                        return;
                    }
                }
            }
            applied = apply_rx.recv(), if !apply_closed => {
                let Some(text) = applied else {
                    apply_closed = true;
                    continue;
                };

                if exceeds_cap(&text) {
                    tracing::debug!(
                        "[Clipboard] inbound text ({} bytes) exceeds the {CLIPBOARD_MAX_LEN}-byte cap — refusing to apply",
                        text.len()
                    );
                    continue;
                }

                // Recorded BEFORE writing: the write itself bumps the
                // clipboard sequence number, and the next poll must already
                // see last_applied set so should_publish() suppresses the
                // echo instead of bouncing this text straight back out.
                last_applied = Some(text.clone());

                if !spawn_blocking(move || write_clipboard_text(&text)).await.unwrap_or(false) {
                    tracing::debug!("[Clipboard] failed to write host clipboard (transiently locked by another app?)");
                }
            }
        }
    }
}

#[cfg(windows)]
fn sequence_number() -> u32 {
    unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
fn sequence_number() -> u32 {
    0
}

/// CF_UNICODETEXT = 13, a stable Win32 ABI constant. Hardcoded instead of
/// pulling in the `Win32_System_Ole` windows-sys feature — the only module
/// that re-exports it — for a single integer.
#[cfg(windows)]
const CF_UNICODETEXT: u32 = 13;

/// Blocking: run inside `spawn_blocking`. OpenClipboard/GlobalLock may
/// transiently fail while another app holds the clipboard — always returns
/// `None` on failure (retry next tick), never panics.
#[cfg(windows)]
fn read_clipboard_text() -> Option<String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }

        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                return None;
            }
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                return None;
            }
            // CF_UNICODETEXT is nul-terminated.
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            GlobalUnlock(handle);
            Some(text)
        })();

        CloseClipboard();
        result
    }
}

#[cfg(not(windows))]
fn read_clipboard_text() -> Option<String> {
    None
}

/// Blocking: run inside `spawn_blocking`. Same transient-failure contract as
/// [`read_clipboard_text`] — returns `false` on any failure, never panics.
#[cfg(windows)]
fn write_clipboard_text(text: &str) -> bool {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };

    // Wide, nul-terminated — CF_UNICODETEXT's expected encoding.
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
    let byte_len = wide.len() * 2; // u16 = 2 bytes each

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }

        let ok = (|| {
            if EmptyClipboard() == 0 {
                return false;
            }

            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len);
            if hmem.is_null() {
                return false;
            }

            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                GlobalFree(hmem);
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            GlobalUnlock(hmem);

            // SetClipboardData takes ownership of hmem on success; freeing it
            // here would be a double-free of memory the system now owns.
            if SetClipboardData(CF_UNICODETEXT, hmem).is_null() {
                GlobalFree(hmem);
                return false;
            }

            true
        })();

        CloseClipboard();
        ok
    }
}

#[cfg(not(windows))]
fn write_clipboard_text(_text: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Wire byte pin (shared with tests/clipboard_wire.test.mjs) ──────────

    #[test]
    fn text_byte_pin() {
        assert_eq!(
            encode_text("hi"),
            vec![0x00, 0x02, 0x00, 0x00, 0x00, 0x68, 0x69]
        );
    }

    #[test]
    fn decode_roundtrip() {
        let encoded = encode_text("hello, world");
        assert_eq!(decode_text(&encoded).as_deref(), Some("hello, world"));
    }

    #[test]
    fn decode_empty_string_roundtrip() {
        let encoded = encode_text("");
        assert_eq!(decode_text(&encoded).as_deref(), Some(""));
    }

    #[test]
    fn decode_rejects_oversized_len() {
        let mut data = vec![CLIPBOARD_KIND_TEXT];
        data.extend_from_slice(&((CLIPBOARD_MAX_LEN + 1) as u32).to_le_bytes());
        assert_eq!(decode_text(&data), None);
    }

    #[test]
    fn decode_rejects_unknown_kind() {
        let data = [0xFFu8, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_text(&data), None);
    }

    #[test]
    fn decode_rejects_truncated() {
        // Header claims 5 bytes of payload but only 2 are present.
        let data = [0x00u8, 0x05, 0x00, 0x00, 0x00, 0x68, 0x69];
        assert_eq!(decode_text(&data), None);
    }

    #[test]
    fn decode_rejects_short_header() {
        assert_eq!(decode_text(&[0x00, 0x02, 0x00]), None);
    }

    #[test]
    fn ws_frame_prefixes_channel_id() {
        let frame = ws_clipboard_frame("hi");
        assert_eq!(frame[0], TransportChannelId::CLIPBOARD);
        assert_eq!(frame[0], 28);
        assert_eq!(&frame[1..], &[0x00, 0x02, 0x00, 0x00, 0x00, 0x68, 0x69]);
    }

    #[test]
    fn exceeds_cap_boundary() {
        assert!(!exceeds_cap(&"a".repeat(CLIPBOARD_MAX_LEN)));
        assert!(exceeds_cap(&"a".repeat(CLIPBOARD_MAX_LEN + 1)));
    }

    // ── should_publish loop guard ───────────────────────────────────────

    #[test]
    fn should_publish_suppresses_applied_echo() {
        assert!(!should_publish("pasted text", Some("pasted text"), None));
    }

    #[test]
    fn should_publish_suppresses_duplicate_send() {
        assert!(!should_publish("same", None, Some("same")));
    }

    #[test]
    fn should_publish_distinguishes_applied_and_sent_independently() {
        assert!(should_publish("new", Some("old applied"), Some("old sent")));
    }

    #[test]
    fn should_publish_allows_first_sample() {
        assert!(should_publish("first", None, None));
    }
}
