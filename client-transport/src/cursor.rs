//! `cursor` DataChannel wire decode + session→shell readback — M4 cursor
//! P1 (docs/design/cursor-channel.md §3).
//!
//! POS message, little-endian, 14 bytes:
//! `u8 kind=0 | u8 visible(0/1) | i32 x | i32 y | u16 vw | u16 vh`
//!
//! `x`,`y` are cursor screen coordinates and `vw`,`vh` the captured
//! monitor's size in the same coordinate space.
//!
//! Mirror: `streamer/src/transport/webrtc/cursor_wire.rs` (encode) and
//! `web/stream/cursor_wire.ts` (parse) — keep the byte-pinned tests in
//! lockstep across all three (qu_wire pattern). This crate only decodes
//! (the client never encodes POS; the host is the sole cursor authority).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const CURSOR_KIND_POS: u8 = 0;
pub const CURSOR_POS_LEN: usize = 14;

/// One cursor state sample (host authority: visibility + position).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub vw: u16,
    pub vh: u16,
}

/// Decode one `cursor` DataChannel message. Returns `None` for buffers
/// shorter than the POS frame or an unrecognised `kind` byte (forward
/// compatible with future message kinds this client doesn't understand
/// yet).
pub fn decode_pos(bytes: &[u8]) -> Option<CursorPos> {
    if bytes.is_empty() || bytes[0] != CURSOR_KIND_POS {
        return None;
    }
    if bytes.len() < CURSOR_POS_LEN {
        return None;
    }
    Some(CursorPos {
        visible: bytes[1] != 0,
        x: i32::from_le_bytes(bytes[2..6].try_into().expect("4-byte slice")),
        y: i32::from_le_bytes(bytes[6..10].try_into().expect("4-byte slice")),
        vw: u16::from_le_bytes(bytes[10..12].try_into().expect("2-byte slice")),
        vh: u16::from_le_bytes(bytes[12..14].try_into().expect("2-byte slice")),
    })
}

/// M4 cursor readback shared with the shell: latest host-reported
/// visibility + position, stored via plain atomics.
///
/// The session's `cursor` DataChannel callback stores directly into this
/// struct (see `session::create_peer`) instead of routing through the
/// `LocalEvent` loop: there is no ordering dependency on any other session
/// state (a stale-by-one-frame position is harmless, unlike e.g. FEC data
/// which must stay ordered with the rest of the receive pipeline), so a
/// bare atomic store from the WebRTC callback thread is both correct and
/// avoids an extra channel hop on the hot path of every mouse-move sample.
///
/// `pos` packs `x` and `y` (both `i32`) into one `u64`: high 32 bits = `x`,
/// low 32 bits = `y`, each reinterpreted through its unsigned bit pattern
/// (`i32 as u32`) so negative coordinates round-trip exactly.
#[derive(Debug)]
pub struct CursorShared {
    visible: AtomicBool,
    pos: AtomicU64,
}

impl Default for CursorShared {
    /// Host cursor is assumed visible until the first POS sample says
    /// otherwise (matches the web `CursorAutoMode`'s starting posture).
    fn default() -> Self {
        Self {
            visible: AtomicBool::new(true),
            pos: AtomicU64::new(0),
        }
    }
}

impl CursorShared {
    /// Store one decoded POS sample (called from the data-channel
    /// callback thread — see the doc comment on this struct for why plain
    /// atomic stores are sufficient here).
    pub fn store(&self, p: CursorPos) {
        self.visible.store(p.visible, Ordering::Release);
        let packed = ((p.x as u32 as u64) << 32) | (p.y as u32 as u64);
        self.pos.store(packed, Ordering::Release);
    }

    /// Host cursor visibility (shell readback for mouse-mode wiring).
    pub fn visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    /// Last reported host cursor position, `(x, y)`.
    pub fn pos(&self) -> (i32, i32) {
        let packed = self.pos.load(Ordering::Acquire);
        ((packed >> 32) as u32 as i32, packed as u32 as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared byte pin with `streamer/src/transport/webrtc/cursor_wire.rs`
    /// and `tests/cursor_wire.test.mjs` — change all three or none.
    #[test]
    fn pos_byte_pin() {
        let bytes: [u8; CURSOR_POS_LEN] = [
            0x00, 0x01, // kind, visible
            0xE8, 0x03, 0x00, 0x00, // x = 1000
            0xFE, 0xFF, 0xFF, 0xFF, // y = -2
            0x00, 0x0A, // vw = 2560
            0xA0, 0x05, // vh = 1440
        ];
        let p = decode_pos(&bytes).expect("decodes");
        assert_eq!(
            p,
            CursorPos {
                visible: true,
                x: 1000,
                y: -2,
                vw: 2560,
                vh: 1440,
            }
        );
    }

    #[test]
    fn hidden_pin() {
        let bytes: [u8; CURSOR_POS_LEN] = [
            0x00, 0x00, // kind, visible = false
            0x00, 0x00, 0x00, 0x00, // x = 0
            0x00, 0x00, 0x00, 0x00, // y = 0
            0x80, 0x07, // vw = 1920
            0x38, 0x04, // vh = 1080
        ];
        let p = decode_pos(&bytes).expect("decodes");
        assert!(!p.visible);
        assert_eq!(p.x, 0);
        assert_eq!(p.y, 0);
        assert_eq!(p.vw, 1920);
        assert_eq!(p.vh, 1080);
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(decode_pos(&[]), None);
    }

    #[test]
    fn unknown_kind_is_none() {
        let mut bytes = [0u8; CURSOR_POS_LEN];
        bytes[0] = 0x01; // no other kind is defined yet
        assert_eq!(decode_pos(&bytes), None);
    }

    #[test]
    fn short_buffer_is_none() {
        // 13 bytes: correct kind, one byte short of a full POS frame.
        let bytes = [0u8; CURSOR_POS_LEN - 1];
        assert_eq!(decode_pos(&bytes), None);
    }

    #[test]
    fn shared_store_readback_roundtrip() {
        let shared = CursorShared::default();
        assert!(shared.visible());
        assert_eq!(shared.pos(), (0, 0));

        shared.store(CursorPos {
            visible: false,
            x: -100,
            y: 2000,
            vw: 1920,
            vh: 1080,
        });
        assert!(!shared.visible());
        assert_eq!(shared.pos(), (-100, 2000));

        shared.store(CursorPos {
            visible: true,
            x: i32::MIN,
            y: i32::MAX,
            vw: 0,
            vh: 0,
        });
        assert!(shared.visible());
        assert_eq!(shared.pos(), (i32::MIN, i32::MAX));
    }
}
