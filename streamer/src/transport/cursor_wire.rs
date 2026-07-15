//! `cursor` DataChannel wire format — M4 cursor P1
//! (docs/design/cursor-channel.md §3).
//!
//! POS message, little-endian, 14 bytes:
//! `u8 kind=0 | u8 visible(0/1) | i32 x | i32 y | u16 vw | u16 vh`
//!
//! `x`,`y` are cursor screen coordinates and `vw`,`vh` the captured
//! monitor's size **in the same coordinate space** (the client maps the
//! normalized position onto the video rect, so only consistency between
//! the two matters, not which DPI space they live in).
//!
//! Mirror: `web/stream/cursor_wire.ts` — keep the byte-pinned tests in
//! lockstep (qu_wire pattern).

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

pub fn encode_pos(p: CursorPos) -> [u8; CURSOR_POS_LEN] {
    let mut out = [0u8; CURSOR_POS_LEN];
    out[0] = CURSOR_KIND_POS;
    out[1] = u8::from(p.visible);
    out[2..6].copy_from_slice(&p.x.to_le_bytes());
    out[6..10].copy_from_slice(&p.y.to_le_bytes());
    out[10..12].copy_from_slice(&p.vw.to_le_bytes());
    out[12..14].copy_from_slice(&p.vh.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared byte pin with `tests/cursor_wire.test.mjs` — change both or
    /// neither.
    #[test]
    fn pos_byte_pin() {
        let p = CursorPos {
            visible: true,
            x: 1000,
            y: -2,
            vw: 2560,
            vh: 1440,
        };
        assert_eq!(
            encode_pos(p),
            [
                0x00, 0x01, // kind, visible
                0xE8, 0x03, 0x00, 0x00, // x = 1000
                0xFE, 0xFF, 0xFF, 0xFF, // y = -2
                0x00, 0x0A, // vw = 2560
                0xA0, 0x05, // vh = 1440
            ]
        );
    }

    #[test]
    fn hidden_pin() {
        let p = CursorPos {
            visible: false,
            x: 0,
            y: 0,
            vw: 1920,
            vh: 1080,
        };
        let b = encode_pos(p);
        assert_eq!(b[0], CURSOR_KIND_POS);
        assert_eq!(b[1], 0);
        assert_eq!(&b[10..12], &1920u16.to_le_bytes());
        assert_eq!(&b[12..14], &1080u16.to_le_bytes());
    }
}
