//! `cursor` DataChannel wire format — M4 cursor P1/P2
//! (docs/design/cursor-channel.md §3, §P2).
//!
//! POS message, little-endian, 18 bytes (v2 — `shape_id` appended in P2;
//! the native client's `decode_pos` only reads the first 14 bytes and
//! rejects shorter buffers/unknown kinds, so this append is wire-compatible):
//! `u8 kind=0 | u8 visible(0/1) | i32 x | i32 y | u16 vw | u16 vh | u32 shape_id`
//!
//! `x`,`y` are cursor screen coordinates and `vw`,`vh` the captured
//! monitor's size **in the same coordinate space** (the client maps the
//! normalized position onto the video rect, so only consistency between
//! the two matters, not which DPI space they live in). `shape_id` is 0 for
//! "unknown/none" and otherwise refers to the last SHAPE message with that
//! id (§P2 below).
//!
//! SHAPE message, little-endian, 17-byte header + PNG payload:
//! `u8 kind=1 | u32 shape_id | u16 w | u16 h | u16 hot_x | u16 hot_y | u32 png_len | png bytes`
//! Sent only when the host cursor's shape (hCursor handle) changes —
//! `cursor_tracker::extract_shape` produces the RGBA PNG via
//! `GetIconInfoExW`/`GetDIBits`. `png_len` (and therefore the whole frame)
//! is capped at [`CURSOR_SHAPE_MAX_PNG_LEN`] bytes on both the encode and
//! parse sides.
//!
//! Mirror: `web/stream/cursor_wire.ts` — keep the byte-pinned tests in
//! lockstep (qu_wire pattern). Unknown kinds are ignored on parse there,
//! matching this side's forward-compat contract.

pub const CURSOR_KIND_POS: u8 = 0;
pub const CURSOR_KIND_SHAPE: u8 = 1;
pub const CURSOR_POS_LEN: usize = 18;
/// Fixed portion of a SHAPE frame, before the variable-length PNG payload.
pub const CURSOR_SHAPE_HEADER_LEN: usize = 17;
/// Refuse to encode (host) / accept (client) a PNG payload bigger than this
/// — bounds a single cursor shape frame regardless of source cursor size.
pub const CURSOR_SHAPE_MAX_PNG_LEN: usize = 262_144;

/// One cursor state sample (host authority: visibility + position + the
/// shape currently in effect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub vw: u16,
    pub vh: u16,
    /// 0 = unknown/none (no shape extracted yet, or extraction failed this
    /// tick); otherwise the id of the last SHAPE message describing the
    /// cursor currently at this position.
    pub shape_id: u32,
}

pub fn encode_pos(p: CursorPos) -> [u8; CURSOR_POS_LEN] {
    let mut out = [0u8; CURSOR_POS_LEN];
    out[0] = CURSOR_KIND_POS;
    out[1] = u8::from(p.visible);
    out[2..6].copy_from_slice(&p.x.to_le_bytes());
    out[6..10].copy_from_slice(&p.y.to_le_bytes());
    out[10..12].copy_from_slice(&p.vw.to_le_bytes());
    out[12..14].copy_from_slice(&p.vh.to_le_bytes());
    out[14..18].copy_from_slice(&p.shape_id.to_le_bytes());
    out
}

/// Encodes a SHAPE frame; `None` if `png` exceeds
/// [`CURSOR_SHAPE_MAX_PNG_LEN`] (refuse-to-send per docs/design/cursor-channel.md §P2).
pub fn encode_shape(
    shape_id: u32,
    w: u16,
    h: u16,
    hot_x: u16,
    hot_y: u16,
    png: &[u8],
) -> Option<Vec<u8>> {
    if png.len() > CURSOR_SHAPE_MAX_PNG_LEN {
        return None;
    }
    let mut out = Vec::with_capacity(CURSOR_SHAPE_HEADER_LEN + png.len());
    out.push(CURSOR_KIND_SHAPE);
    out.extend_from_slice(&shape_id.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&hot_x.to_le_bytes());
    out.extend_from_slice(&hot_y.to_le_bytes());
    out.extend_from_slice(&(png.len() as u32).to_le_bytes());
    out.extend_from_slice(png);
    Some(out)
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
            shape_id: 7,
        };
        assert_eq!(
            encode_pos(p),
            [
                0x00, 0x01, // kind, visible
                0xE8, 0x03, 0x00, 0x00, // x = 1000
                0xFE, 0xFF, 0xFF, 0xFF, // y = -2
                0x00, 0x0A, // vw = 2560
                0xA0, 0x05, // vh = 1440
                0x07, 0x00, 0x00, 0x00, // shape_id = 7
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
            shape_id: 0,
        };
        let b = encode_pos(p);
        assert_eq!(b[0], CURSOR_KIND_POS);
        assert_eq!(b[1], 0);
        assert_eq!(&b[10..12], &1920u16.to_le_bytes());
        assert_eq!(&b[12..14], &1080u16.to_le_bytes());
        assert_eq!(&b[14..18], &0u32.to_le_bytes());
    }

    /// Shared byte pin with `tests/cursor_wire.test.mjs` — change both or
    /// neither.
    #[test]
    fn shape_byte_pin() {
        let png = [0xAAu8, 0xBB, 0xCC];
        let out = encode_shape(7, 32, 32, 3, 4, &png).expect("under cap");
        assert_eq!(
            out,
            [
                0x01, // kind
                0x07, 0x00, 0x00, 0x00, // shape_id = 7
                0x20, 0x00, // w = 32
                0x20, 0x00, // h = 32
                0x03, 0x00, // hot_x = 3
                0x04, 0x00, // hot_y = 4
                0x03, 0x00, 0x00, 0x00, // png_len = 3
                0xAA, 0xBB, 0xCC, // png bytes
            ]
        );
    }

    #[test]
    fn shape_refuses_over_cap() {
        let png = vec![0u8; CURSOR_SHAPE_MAX_PNG_LEN + 1];
        assert!(encode_shape(1, 1, 1, 0, 0, &png).is_none());
    }

    #[test]
    fn shape_accepts_exactly_at_cap() {
        let png = vec![0u8; CURSOR_SHAPE_MAX_PNG_LEN];
        assert!(encode_shape(1, 1, 1, 0, 0, &png).is_some());
    }
}
