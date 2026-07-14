//! Pure (no async, no I/O) wire/framing module for the `video_qu` DataChannel.
//!
//! All integers are little-endian.  See `docs/design/qu-protocol.md §2`.
//!
//! Host → client kinds are in the `0x0x` band; client → host in the `0x8x` band.

// ── Constants ─────────────────────────────────────────────────────────────

pub const KIND_QU_CONFIG: u8    = 0x01;
pub const KIND_QU_TILE: u8      = 0x02;
pub const KIND_QU_INVALIDATE: u8 = 0x03;
pub const KIND_QU_EPOCH: u8     = 0x04;
pub const KIND_QU_SUBSCRIBE: u8 = 0x81;
pub const KIND_QU_BUDGET: u8    = 0x82;

/// Fixed byte lengths (header only, before any variable payload).
const CONFIG_LEN: usize      = 13; // 1 + 2+2+2+2 + 4
const TILE_HDR: usize        = 19; // 1 + 4 + 2+2 + 1+1 + 4 + 4
const INVALIDATE_HDR: usize  = 7;  // 1 + 4 + 2
const EPOCH_LEN: usize       = 5;  // 1 + 4
const SUBSCRIBE_LEN: usize   = 2;  // 1 + 1
const BUDGET_LEN: usize      = 5;  // 1 + 4

// ── Message enum ──────────────────────────────────────────────────────────

/// Parsed QU message.  All integers in the wire format are little-endian.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuMsg {
    Config {
        tile_w:    u16,
        tile_h:    u16,
        grid_cols: u16,
        grid_rows: u16,
        epoch:     u32,
    },
    Tile {
        epoch:       u32,
        col:         u16,
        row:         u16,
        format:      u8,
        flags:       u8,
        crc32_bgra:  u32,
        payload:     Vec<u8>,
    },
    Invalidate {
        epoch: u32,
        tiles: Vec<(u16, u16)>,
    },
    Epoch {
        new_epoch: u32,
    },
    Subscribe {
        version: u8,
    },
    Budget {
        kbps: u32,
    },
}

// ── Encode ────────────────────────────────────────────────────────────────

/// `0x01 QU_CONFIG`: tile_w(u16) | tile_h(u16) | grid_cols(u16) | grid_rows(u16) | epoch(u32)
// Wire API: used by in-module tests and the M6 native client / TS injector.
#[allow(dead_code)]
pub fn encode_config(
    tile_w: u16,
    tile_h: u16,
    grid_cols: u16,
    grid_rows: u16,
    epoch: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(CONFIG_LEN);
    buf.push(KIND_QU_CONFIG);
    buf.extend_from_slice(&tile_w.to_le_bytes());
    buf.extend_from_slice(&tile_h.to_le_bytes());
    buf.extend_from_slice(&grid_cols.to_le_bytes());
    buf.extend_from_slice(&grid_rows.to_le_bytes());
    buf.extend_from_slice(&epoch.to_le_bytes());
    buf
}

/// `0x02 QU_TILE`: epoch(u32) | col(u16) | row(u16) | format(u8) | flags(u8) |
///                 crc32_bgra(u32) | payload_len(u32) | payload
// Wire API: used by in-module tests and the M6 native client / TS injector.
#[allow(dead_code)]
pub fn encode_tile(
    epoch:      u32,
    col:        u16,
    row:        u16,
    format:     u8,
    flags:      u8,
    crc32_bgra: u32,
    payload:    &[u8],
) -> Vec<u8> {
    let payload_len = payload.len() as u32;
    let mut buf = Vec::with_capacity(TILE_HDR + payload.len());
    buf.push(KIND_QU_TILE);
    buf.extend_from_slice(&epoch.to_le_bytes());
    buf.extend_from_slice(&col.to_le_bytes());
    buf.extend_from_slice(&row.to_le_bytes());
    buf.push(format);
    buf.push(flags);
    buf.extend_from_slice(&crc32_bgra.to_le_bytes());
    buf.extend_from_slice(&payload_len.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// `0x03 QU_INVALIDATE`: epoch(u32) | count(u16) | count×(col(u16)|row(u16))
// Wire API: used by in-module tests and the M6 native client / TS injector.
#[allow(dead_code)]
pub fn encode_invalidate(epoch: u32, tiles: &[(u16, u16)]) -> Vec<u8> {
    let count = tiles.len() as u16;
    let mut buf = Vec::with_capacity(INVALIDATE_HDR + tiles.len() * 4);
    buf.push(KIND_QU_INVALIDATE);
    buf.extend_from_slice(&epoch.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    for (col, row) in tiles {
        buf.extend_from_slice(&col.to_le_bytes());
        buf.extend_from_slice(&row.to_le_bytes());
    }
    buf
}

/// `0x04 QU_EPOCH`: new_epoch(u32)
// Wire API: used by in-module tests and the M6 native client / TS injector.
#[allow(dead_code)]
pub fn encode_epoch(new_epoch: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(EPOCH_LEN);
    buf.push(KIND_QU_EPOCH);
    buf.extend_from_slice(&new_epoch.to_le_bytes());
    buf
}

/// `0x81 QU_SUBSCRIBE`: version(u8)
// Wire API: used by in-module tests and the M6 native client / TS injector.
#[allow(dead_code)]
pub fn encode_subscribe(version: u8) -> Vec<u8> {
    vec![KIND_QU_SUBSCRIBE, version]
}

/// `0x82 QU_BUDGET`: kbps(u32)
// Wire API: used by in-module tests and the M6 native client / TS injector.
#[allow(dead_code)]
pub fn encode_budget(kbps: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(BUDGET_LEN);
    buf.push(KIND_QU_BUDGET);
    buf.extend_from_slice(&kbps.to_le_bytes());
    buf
}

// ── Parse ─────────────────────────────────────────────────────────────────

/// Parse a `video_qu` wire message.  Returns `None` on:
///   - empty buffer (no kind byte),
///   - truncated message (too short for the declared header),
///   - `QU_TILE` where `payload_len` exceeds remaining bytes,
///   - `QU_INVALIDATE` where `count` × 4 bytes are not present,
///   - unknown `msg_kind` (use [`peek`] to inspect kind without full parse).
pub fn parse_msg(buf: &[u8]) -> Option<QuMsg> {
    let kind = *buf.first()?;
    match kind {
        KIND_QU_CONFIG => {
            if buf.len() < CONFIG_LEN { return None; }
            Some(QuMsg::Config {
                tile_w:    u16::from_le_bytes(buf[1..3].try_into().ok()?),
                tile_h:    u16::from_le_bytes(buf[3..5].try_into().ok()?),
                grid_cols: u16::from_le_bytes(buf[5..7].try_into().ok()?),
                grid_rows: u16::from_le_bytes(buf[7..9].try_into().ok()?),
                epoch:     u32::from_le_bytes(buf[9..13].try_into().ok()?),
            })
        }
        KIND_QU_TILE => {
            if buf.len() < TILE_HDR { return None; }
            let payload_len =
                u32::from_le_bytes(buf[15..19].try_into().ok()?) as usize;
            if buf.len() < TILE_HDR + payload_len { return None; }
            Some(QuMsg::Tile {
                epoch:      u32::from_le_bytes(buf[1..5].try_into().ok()?),
                col:        u16::from_le_bytes(buf[5..7].try_into().ok()?),
                row:        u16::from_le_bytes(buf[7..9].try_into().ok()?),
                format:     buf[9],
                flags:      buf[10],
                crc32_bgra: u32::from_le_bytes(buf[11..15].try_into().ok()?),
                payload:    buf[19..19 + payload_len].to_vec(),
            })
        }
        KIND_QU_INVALIDATE => {
            if buf.len() < INVALIDATE_HDR { return None; }
            let epoch = u32::from_le_bytes(buf[1..5].try_into().ok()?);
            let count = u16::from_le_bytes(buf[5..7].try_into().ok()?) as usize;
            let expected = INVALIDATE_HDR + count * 4;
            if buf.len() < expected { return None; }
            let mut tiles = Vec::with_capacity(count);
            for i in 0..count {
                let off = INVALIDATE_HDR + i * 4;
                tiles.push((
                    u16::from_le_bytes(buf[off..off + 2].try_into().ok()?),
                    u16::from_le_bytes(buf[off + 2..off + 4].try_into().ok()?),
                ));
            }
            Some(QuMsg::Invalidate { epoch, tiles })
        }
        KIND_QU_EPOCH => {
            if buf.len() < EPOCH_LEN { return None; }
            Some(QuMsg::Epoch {
                new_epoch: u32::from_le_bytes(buf[1..5].try_into().ok()?),
            })
        }
        KIND_QU_SUBSCRIBE => {
            if buf.len() < SUBSCRIBE_LEN { return None; }
            Some(QuMsg::Subscribe { version: buf[1] })
        }
        KIND_QU_BUDGET => {
            if buf.len() < BUDGET_LEN { return None; }
            Some(QuMsg::Budget {
                kbps: u32::from_le_bytes(buf[1..5].try_into().ok()?),
            })
        }
        _ => None,
    }
}

// ── Peek ─────────────────────────────────────────────────────────────────

/// Light, non-allocating peek: returns `(kind, Option<epoch>)`.
///
/// `epoch` is extracted only for `QU_TILE` (0x02) and `QU_INVALIDATE` (0x03),
/// which both carry epoch at bytes `[1..5]` of the message.  For all other kinds,
/// `epoch` is `None`.
///
/// Returns `None` only when `buf` is empty (cannot read the kind byte).  An
/// unknown or client-band kind is returned with its raw byte value so the relay
/// can make forward-compat routing decisions without a full parse.
pub fn peek(buf: &[u8]) -> Option<(u8, Option<u32>)> {
    let kind = *buf.first()?;
    let epoch = match kind {
        KIND_QU_TILE | KIND_QU_INVALIDATE if buf.len() >= 5 => {
            buf[1..5].try_into().ok().map(u32::from_le_bytes)
        }
        _ => None,
    };
    Some((kind, epoch))
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Roundtrip: all six message kinds ─────────────────────────────────

    #[test]
    fn config_roundtrip() {
        let orig = QuMsg::Config { tile_w: 128, tile_h: 128, grid_cols: 15, grid_rows: 9, epoch: 7 };
        let wire = encode_config(128, 128, 15, 9, 7);
        assert_eq!(parse_msg(&wire), Some(orig));
    }

    #[test]
    fn tile_roundtrip() {
        let payload = vec![0xAA, 0xBB];
        let orig = QuMsg::Tile {
            epoch: 7, col: 3, row: 2, format: 0, flags: 0,
            crc32_bgra: 0xDEAD_BEEF, payload: payload.clone(),
        };
        let wire = encode_tile(7, 3, 2, 0, 0, 0xDEAD_BEEF, &payload);
        assert_eq!(parse_msg(&wire), Some(orig));
    }

    #[test]
    fn invalidate_roundtrip() {
        let tiles = vec![(1u16, 2u16), (3u16, 4u16)];
        let orig = QuMsg::Invalidate { epoch: 5, tiles: tiles.clone() };
        let wire = encode_invalidate(5, &tiles);
        assert_eq!(parse_msg(&wire), Some(orig));
    }

    #[test]
    fn epoch_roundtrip() {
        let orig = QuMsg::Epoch { new_epoch: 42 };
        let wire = encode_epoch(42);
        assert_eq!(parse_msg(&wire), Some(orig));
    }

    #[test]
    fn subscribe_roundtrip() {
        let orig = QuMsg::Subscribe { version: 1 };
        let wire = encode_subscribe(1);
        assert_eq!(parse_msg(&wire), Some(orig));
    }

    #[test]
    fn budget_roundtrip() {
        let orig = QuMsg::Budget { kbps: 4000 };
        let wire = encode_budget(4000);
        assert_eq!(parse_msg(&wire), Some(orig));
    }

    // ── Shared byte pins (normative from qu-protocol.md) ─────────────────

    /// QU_CONFIG tile 128×128, cols 15, rows 9, epoch 7
    #[test]
    fn byte_pin_config() {
        let wire = encode_config(128, 128, 15, 9, 7);
        let expected: &[u8] = &[
            0x01,
            0x80, 0x00,  // tile_w = 128
            0x80, 0x00,  // tile_h = 128
            0x0F, 0x00,  // grid_cols = 15
            0x09, 0x00,  // grid_rows = 9
            0x07, 0x00, 0x00, 0x00,  // epoch = 7
        ];
        assert_eq!(wire.as_slice(), expected, "QU_CONFIG byte pin mismatch");
    }

    /// QU_TILE epoch 7, col 3, row 2, format 0, flags 0, crc 0xDEADBEEF, payload [0xAA,0xBB]
    #[test]
    fn byte_pin_tile() {
        let wire = encode_tile(7, 3, 2, 0, 0, 0xDEAD_BEEF, &[0xAA, 0xBB]);
        let expected: &[u8] = &[
            0x02,
            0x07, 0x00, 0x00, 0x00,  // epoch = 7
            0x03, 0x00,              // col = 3
            0x02, 0x00,              // row = 2
            0x00,                    // format = 0
            0x00,                    // flags = 0
            0xEF, 0xBE, 0xAD, 0xDE, // crc32_bgra = 0xDEADBEEF LE
            0x02, 0x00, 0x00, 0x00, // payload_len = 2
            0xAA, 0xBB,             // payload
        ];
        assert_eq!(wire.as_slice(), expected, "QU_TILE byte pin mismatch");
    }

    /// QU_SUBSCRIBE version 1
    #[test]
    fn byte_pin_subscribe() {
        let wire = encode_subscribe(1);
        assert_eq!(wire.as_slice(), &[0x81, 0x01], "QU_SUBSCRIBE byte pin mismatch");
    }

    /// QU_BUDGET 4000 kbps
    #[test]
    fn byte_pin_budget() {
        let wire = encode_budget(4000);
        let expected: &[u8] = &[0x82, 0xA0, 0x0F, 0x00, 0x00];
        assert_eq!(wire.as_slice(), expected, "QU_BUDGET byte pin mismatch");
    }

    // ── Truncation domain: QU_CONFIG ─────────────────────────────────────

    #[test]
    fn config_empty_is_none() { assert!(parse_msg(&[]).is_none()); }

    #[test]
    fn config_one_byte_is_none() { assert!(parse_msg(&[KIND_QU_CONFIG]).is_none()); }

    #[test]
    fn config_header_minus_1_is_none() {
        // 13 bytes needed; feed 12
        assert!(parse_msg(&[KIND_QU_CONFIG; 12]).is_none());
    }

    // ── Truncation domain: QU_TILE ────────────────────────────────────────

    #[test]
    fn tile_empty_is_none() { assert!(parse_msg(&[]).is_none()); }

    #[test]
    fn tile_one_byte_is_none() { assert!(parse_msg(&[KIND_QU_TILE]).is_none()); }

    #[test]
    fn tile_header_minus_1_is_none() {
        // 19 bytes needed; feed 18
        assert!(parse_msg(&vec![KIND_QU_TILE; 18]).is_none());
    }

    /// payload_len field claims more bytes than are present → None
    #[test]
    fn tile_payload_len_mismatch_is_none() {
        let mut wire = encode_tile(1, 0, 0, 0, 0, 0, &[0xAA, 0xBB]);
        // Overwrite payload_len to claim 100 bytes; actual payload is 2
        let len_off = 15;
        wire[len_off..len_off + 4].copy_from_slice(&100u32.to_le_bytes());
        assert!(parse_msg(&wire).is_none(), "payload_len mismatch must be rejected");
    }

    // ── Truncation domain: QU_INVALIDATE ─────────────────────────────────

    #[test]
    fn invalidate_empty_is_none() { assert!(parse_msg(&[]).is_none()); }

    #[test]
    fn invalidate_one_byte_is_none() { assert!(parse_msg(&[KIND_QU_INVALIDATE]).is_none()); }

    #[test]
    fn invalidate_header_minus_1_is_none() {
        // 7 bytes needed; feed 6
        assert!(parse_msg(&vec![KIND_QU_INVALIDATE; 6]).is_none());
    }

    /// count claims 1 tile entry but zero pair bytes follow → None
    #[test]
    fn invalidate_count_mismatch_is_none() {
        let mut wire = encode_invalidate(3, &[(0, 0)]);
        // Raise count to 2, but only 1 tile's bytes are present
        wire[5..7].copy_from_slice(&2u16.to_le_bytes());
        assert!(parse_msg(&wire).is_none(), "count mismatch must be rejected");
    }

    // ── Truncation domain: QU_EPOCH ───────────────────────────────────────

    #[test]
    fn epoch_empty_is_none() { assert!(parse_msg(&[]).is_none()); }

    #[test]
    fn epoch_one_byte_is_none() { assert!(parse_msg(&[KIND_QU_EPOCH]).is_none()); }

    #[test]
    fn epoch_header_minus_1_is_none() {
        // 5 bytes needed; feed 4
        assert!(parse_msg(&[KIND_QU_EPOCH; 4]).is_none());
    }

    // ── Truncation domain: QU_SUBSCRIBE ──────────────────────────────────

    #[test]
    fn subscribe_empty_is_none() { assert!(parse_msg(&[]).is_none()); }

    #[test]
    fn subscribe_one_byte_is_none() { assert!(parse_msg(&[KIND_QU_SUBSCRIBE]).is_none()); }

    // ── Truncation domain: QU_BUDGET ─────────────────────────────────────

    #[test]
    fn budget_empty_is_none() { assert!(parse_msg(&[]).is_none()); }

    #[test]
    fn budget_one_byte_is_none() { assert!(parse_msg(&[KIND_QU_BUDGET]).is_none()); }

    #[test]
    fn budget_header_minus_1_is_none() {
        // 5 bytes needed; feed 4
        assert!(parse_msg(&[KIND_QU_BUDGET; 4]).is_none());
    }

    // ── Unknown kind → None ───────────────────────────────────────────────

    #[test]
    fn unknown_kind_is_none() {
        assert!(parse_msg(&[0x05, 0x00, 0x00, 0x00, 0x00]).is_none());
    }

    // ── peek ──────────────────────────────────────────────────────────────

    #[test]
    fn peek_empty_is_none() { assert!(peek(&[]).is_none()); }

    #[test]
    fn peek_tile_extracts_epoch() {
        let wire = encode_tile(99, 0, 0, 0, 0, 0, &[]);
        let (kind, epoch) = peek(&wire).unwrap();
        assert_eq!(kind, KIND_QU_TILE);
        assert_eq!(epoch, Some(99));
    }

    #[test]
    fn peek_invalidate_extracts_epoch() {
        let wire = encode_invalidate(55, &[]);
        let (kind, epoch) = peek(&wire).unwrap();
        assert_eq!(kind, KIND_QU_INVALIDATE);
        assert_eq!(epoch, Some(55));
    }

    #[test]
    fn peek_config_no_epoch() {
        let wire = encode_config(128, 128, 15, 9, 7);
        let (kind, epoch) = peek(&wire).unwrap();
        assert_eq!(kind, KIND_QU_CONFIG);
        assert_eq!(epoch, None);
    }

    #[test]
    fn peek_subscribe_no_epoch() {
        let wire = encode_subscribe(1);
        let (kind, epoch) = peek(&wire).unwrap();
        assert_eq!(kind, KIND_QU_SUBSCRIBE);
        assert_eq!(epoch, None);
    }

    #[test]
    fn peek_unknown_kind_returned() {
        let buf = &[0x05u8, 0x00];
        let (kind, epoch) = peek(buf).unwrap();
        assert_eq!(kind, 0x05);
        assert_eq!(epoch, None);
    }

    /// QU_TILE with only 1-byte buffer: epoch field not present → epoch is None
    #[test]
    fn peek_tile_truncated_no_epoch() {
        let (kind, epoch) = peek(&[KIND_QU_TILE]).unwrap();
        assert_eq!(kind, KIND_QU_TILE);
        assert_eq!(epoch, None); // too short for epoch field
    }
}
