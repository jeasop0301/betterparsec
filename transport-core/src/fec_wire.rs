//! Pure (no async, no I/O) wire/framing module for the `video_fec` and
//! `video_fec_ack` DataChannels.
//!
//! All integers are little-endian.  See `docs/design/fec-framing.md §2`.

use crate::fec;

// ── Constants ─────────────────────────────────────────────────────────────

/// Maximum on-wire size for a single `video_fec` message (source symbol).
/// Repair symbols may slightly exceed this; that is accepted and documented.
// Wire-spec constant: production budget lives in CHUNK_FRAGMENT_MAX; this is
// referenced by tests and the M6 native client (fec-framing.md §8).
#[allow(dead_code)]
pub const FEC_MSG_MAX: usize = 1200;

/// Byte length of the chunk header (frame_id u32 + chunk_index u16 +
/// chunk_count u16 + frame_type u8 + timestamp_us u32 = 13).
pub const CHUNK_HEADER_LEN: usize = 13;

/// Maximum Annex-B fragment that fits in one source symbol message:
/// FEC_MSG_MAX(1200) − source-symbol-hdr(5) − chunk-header(13) = 1182.
pub const CHUNK_FRAGMENT_MAX: usize = 1182;

// ── Chunk layer ───────────────────────────────────────────────────────────

/// Parsed chunk header fields.
// Receive-side wire API: the production consumer is the TS client
// (web/stream/video/fec_wire.ts); the Rust parse half is exercised by the
// in-module + cross-vector tests and is the contract for the M6 native
// client's cdylib receiver (m6-native-spike.md Option-3).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkHeader {
    pub frame_id: u32,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub frame_type_key: bool,
    pub timestamp_us: u32,
}

/// Split `data` into chunk payloads (13-byte header + ≤ 1182-byte fragment).
///
/// Empty `data` produces exactly one chunk with an empty fragment.
/// Panics (debug-assert) if `chunk_count` would exceed `u16::MAX`.
pub fn chunk_frame(
    frame_id: u32,
    frame_type_key: bool,
    timestamp_us: u32,
    data: &[u8],
) -> Vec<Vec<u8>> {
    if data.is_empty() {
        // Spec: "Empty frame data -> exactly one chunk with empty fragment."
        return vec![encode_chunk(frame_id, 0, 1, frame_type_key, timestamp_us, &[])];
    }

    let chunk_count = data.len().div_ceil(CHUNK_FRAGMENT_MAX);
    debug_assert!(
        chunk_count <= u16::MAX as usize,
        "chunk_count {chunk_count} overflows u16"
    );
    let chunk_count_u16 = chunk_count as u16;

    data.chunks(CHUNK_FRAGMENT_MAX)
        .enumerate()
        .map(|(i, fragment)| {
            encode_chunk(
                frame_id,
                i as u16,
                chunk_count_u16,
                frame_type_key,
                timestamp_us,
                fragment,
            )
        })
        .collect()
}

/// Serialise a chunk header + fragment into a single allocation.
///
/// Wire layout (all LE):
/// ```text
/// [0..4]  frame_id      u32
/// [4..6]  chunk_index   u16
/// [6..8]  chunk_count   u16
/// [8]     frame_type    u8  (0=delta, 1=key)
/// [9..13] timestamp_us  u32
/// [13..]  fragment      bytes
/// ```
fn encode_chunk(
    frame_id: u32,
    chunk_index: u16,
    chunk_count: u16,
    frame_type_key: bool,
    timestamp_us: u32,
    fragment: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(CHUNK_HEADER_LEN + fragment.len());
    buf.extend_from_slice(&frame_id.to_le_bytes());        // [0..4]
    buf.extend_from_slice(&chunk_index.to_le_bytes());     // [4..6]
    buf.extend_from_slice(&chunk_count.to_le_bytes());     // [6..8]
    buf.push(if frame_type_key { 1u8 } else { 0u8 });     // [8]
    buf.extend_from_slice(&timestamp_us.to_le_bytes());    // [9..13]
    buf.extend_from_slice(fragment);                       // [13..]
    buf
}

/// Parse the first 13 bytes as a [`ChunkHeader`] and return the remaining
/// bytes as the fragment.  Returns `None` if the slice is shorter than 13.
#[allow(dead_code)] // receive-side wire API: tests + M6 native client (fec-framing.md §8)
pub fn parse_chunk_header(buf: &[u8]) -> Option<(ChunkHeader, &[u8])> {
    if buf.len() < CHUNK_HEADER_LEN {
        return None;
    }
    let frame_id = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    let chunk_index = u16::from_le_bytes(buf[4..6].try_into().ok()?);
    let chunk_count = u16::from_le_bytes(buf[6..8].try_into().ok()?);
    let frame_type_key = buf[8] != 0;
    let timestamp_us = u32::from_le_bytes(buf[9..13].try_into().ok()?);
    let header = ChunkHeader { frame_id, chunk_index, chunk_count, frame_type_key, timestamp_us };
    Some((header, &buf[CHUNK_HEADER_LEN..]))
}

// ── Symbol messages ───────────────────────────────────────────────────────

/// Serialise a [`fec::Symbol`] to its `video_fec` wire bytes.
///
/// Source:  `[0x00] ++ seq(4 LE) ++ chunk_payload`
/// Repair:  `[0x01] ++ repair_seq(2 LE) ++ window_base(4 LE) ++ window_end(4 LE) ++ payload`
pub fn encode_symbol_msg(sym: &fec::Symbol) -> Vec<u8> {
    match sym {
        fec::Symbol::Source { seq, payload } => {
            let mut buf = Vec::with_capacity(1 + 4 + payload.len());
            buf.push(0u8);                                  // kind = 0
            buf.extend_from_slice(&seq.to_le_bytes());      // seq (4 LE)
            buf.extend_from_slice(payload);                 // chunk payload
            buf
        }
        fec::Symbol::Repair { repair_seq, window_base, window_end, payload } => {
            let mut buf = Vec::with_capacity(1 + 2 + 4 + 4 + payload.len());
            buf.push(1u8);                                       // kind = 1
            buf.extend_from_slice(&repair_seq.to_le_bytes());    // repair_seq (2 LE)
            buf.extend_from_slice(&window_base.to_le_bytes());   // window_base (4 LE)
            buf.extend_from_slice(&window_end.to_le_bytes());    // window_end (4 LE)
            buf.extend_from_slice(payload);                      // combination payload
            buf
        }
    }
}

/// Deserialise a `video_fec` wire message into a [`fec::Symbol`].
/// Returns `None` on truncated or unknown-kind input.
#[allow(dead_code)] // receive-side wire API: tests + M6 native client (fec-framing.md §8)
pub fn parse_symbol_msg(buf: &[u8]) -> Option<fec::Symbol> {
    let kind = *buf.first()?;
    match kind {
        0 => {
            // Source: need at least 5 bytes (kind + seq)
            if buf.len() < 5 {
                return None;
            }
            let seq = u32::from_le_bytes(buf[1..5].try_into().ok()?);
            let payload = buf[5..].to_vec();
            Some(fec::Symbol::Source { seq, payload })
        }
        1 => {
            // Repair: need at least 11 bytes (kind + repair_seq + window_base + window_end)
            if buf.len() < 11 {
                return None;
            }
            let repair_seq = u16::from_le_bytes(buf[1..3].try_into().ok()?);
            let window_base = u32::from_le_bytes(buf[3..7].try_into().ok()?);
            let window_end = u32::from_le_bytes(buf[7..11].try_into().ok()?);
            let payload = buf[11..].to_vec();
            Some(fec::Symbol::Repair { repair_seq, window_base, window_end, payload })
        }
        _ => None,
    }
}

// ── ACK messages ──────────────────────────────────────────────────────────

/// Messages sent from client → host on the `video_fec_ack` channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckMsg {
    /// Length-1 message 0x01: client subscribes / activates host FEC sender.
    Subscribe,
    /// Length-1 message 0x00: unrecoverable loss — request a keyframe.
    NeedsIdr,
    /// Length-4 message: highest fully decoded source seq (u32 LE).
    Ack(u32),
}

/// Parse a raw `video_fec_ack` message.
///
/// | Wire bytes | Meaning |
/// |---|---|
/// | `[0x01]` | Subscribe — client activates host FEC sender |
/// | `[0x00]` | NeedsIdr — unrecoverable loss, request keyframe |
/// | 4 bytes   | Ack(u32 LE) — highest fully decoded source seq |
///
/// Returns `None` for empty, ambiguous length (2, 3, ≥5), or an
/// unknown single-byte value (anything other than 0x00 / 0x01).
pub fn parse_ack_msg(buf: &[u8]) -> Option<AckMsg> {
    match buf.len() {
        0 => None,
        1 => match buf[0] {
            0x00 => Some(AckMsg::NeedsIdr),
            0x01 => Some(AckMsg::Subscribe),
            _    => None,
        },
        4 => {
            let seq = u32::from_le_bytes(buf[0..4].try_into().ok()?);
            Some(AckMsg::Ack(seq))
        }
        _ => None,
    }
}

/// Encode an [`AckMsg`] to its wire bytes.
#[allow(dead_code)] // receive-side wire API: tests + M6 native client (fec-framing.md §8)
pub fn encode_ack_msg(msg: &AckMsg) -> Vec<u8> {
    match msg {
        AckMsg::Subscribe => vec![0x01],
        AckMsg::NeedsIdr  => vec![0x00],
        AckMsg::Ack(seq)  => seq.to_le_bytes().to_vec(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::{FecConfig, FecEncoder, Symbol};

    // ── Symbol roundtrip ──────────────────────────────────────────────────

    #[test]
    fn source_symbol_roundtrip() {
        let sym = Symbol::Source { seq: 42, payload: vec![0xDE, 0xAD, 0xBE, 0xEF] };
        let wire = encode_symbol_msg(&sym);
        let parsed = parse_symbol_msg(&wire).expect("parse failed");
        assert_eq!(sym, parsed);
    }

    #[test]
    fn repair_symbol_roundtrip() {
        let sym = Symbol::Repair {
            repair_seq: 7,
            window_base: 3,
            window_end: 10,
            payload: vec![0x11, 0x22, 0x33],
        };
        let wire = encode_symbol_msg(&sym);
        let parsed = parse_symbol_msg(&wire).expect("parse failed");
        assert_eq!(sym, parsed);
    }

    #[test]
    fn source_symbol_empty_payload_roundtrip() {
        let sym = Symbol::Source { seq: 0, payload: vec![] };
        let wire = encode_symbol_msg(&sym);
        let parsed = parse_symbol_msg(&wire).expect("parse failed");
        assert_eq!(sym, parsed);
    }

    // ── Byte-level pin: source symbol ─────────────────────────────────────
    //
    // Source { seq=1, payload=[0x42,0x43] }
    // Expected: [0x00, 0x01,0x00,0x00,0x00, 0x42,0x43]
    #[test]
    fn source_symbol_byte_pin() {
        let sym = Symbol::Source { seq: 1, payload: vec![0x42, 0x43] };
        let wire = encode_symbol_msg(&sym);
        let expected: &[u8] = &[0x00, 0x01, 0x00, 0x00, 0x00, 0x42, 0x43];
        assert_eq!(wire.as_slice(), expected, "source symbol byte layout mismatch");
    }

    // ── Byte-level pin: repair symbol ─────────────────────────────────────
    //
    // Repair { repair_seq=2, window_base=0, window_end=1, payload=[0xAB,0xCD] }
    // Expected: [0x01, 0x02,0x00, 0x00,0x00,0x00,0x00, 0x01,0x00,0x00,0x00, 0xAB,0xCD]
    #[test]
    fn repair_symbol_byte_pin() {
        let sym = Symbol::Repair {
            repair_seq: 2,
            window_base: 0,
            window_end: 1,
            payload: vec![0xAB, 0xCD],
        };
        let wire = encode_symbol_msg(&sym);
        let expected: &[u8] = &[
            0x01,                         // kind=1
            0x02, 0x00,                   // repair_seq=2 LE
            0x00, 0x00, 0x00, 0x00,       // window_base=0 LE
            0x01, 0x00, 0x00, 0x00,       // window_end=1 LE
            0xAB, 0xCD,                   // payload
        ];
        assert_eq!(wire.as_slice(), expected, "repair symbol byte layout mismatch");
    }

    // ── parse_symbol_msg: truncated / unknown kind → None ─────────────────

    #[test]
    fn parse_symbol_msg_empty_is_none() {
        assert!(parse_symbol_msg(&[]).is_none());
    }

    #[test]
    fn parse_symbol_msg_unknown_kind_is_none() {
        assert!(parse_symbol_msg(&[0x02, 0x00, 0x00, 0x00, 0x00]).is_none());
    }

    #[test]
    fn parse_symbol_msg_source_truncated_is_none() {
        // Only 4 bytes: kind + 3 seq bytes (need 5)
        assert!(parse_symbol_msg(&[0x00, 0x01, 0x00, 0x00]).is_none());
    }

    #[test]
    fn parse_symbol_msg_repair_truncated_is_none() {
        // Only 10 bytes: kind + 2 + 4 + 3 (need 11)
        assert!(parse_symbol_msg(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).is_none());
    }

    // ── Chunk header byte-level pin ───────────────────────────────────────
    //
    // frame_id=1, chunk_index=0, chunk_count=1, key=true, timestamp_us=0x11223344
    // Expected: [0x01,0x00,0x00,0x00, 0x00,0x00, 0x01,0x00, 0x01, 0x44,0x33,0x22,0x11]
    #[test]
    fn chunk_header_byte_pin() {
        let chunks = chunk_frame(1, true, 0x1122_3344, &[0xFF]);
        assert_eq!(chunks.len(), 1);
        let hdr = &chunks[0][..CHUNK_HEADER_LEN];
        let expected: &[u8] = &[
            0x01, 0x00, 0x00, 0x00,  // frame_id=1 LE
            0x00, 0x00,              // chunk_index=0 LE
            0x01, 0x00,              // chunk_count=1 LE
            0x01,                    // frame_type=key
            0x44, 0x33, 0x22, 0x11, // timestamp_us LE
        ];
        assert_eq!(hdr, expected, "chunk header byte layout mismatch");
    }

    // ── Chunk boundary domain ─────────────────────────────────────────────

    fn assert_chunk_boundaries(data_size: usize, expected_count: usize, expected_last_frag: usize) {
        let data: Vec<u8> = (0..data_size).map(|i| i as u8).collect();
        let chunks = chunk_frame(0, false, 0, &data);
        assert_eq!(
            chunks.len(), expected_count,
            "data_size={data_size}: expected {expected_count} chunks, got {}",
            chunks.len()
        );
        // Verify chunk_count field in each header
        for (i, chunk) in chunks.iter().enumerate() {
            let (hdr, frag) = parse_chunk_header(chunk).expect("header parse failed");
            assert_eq!(
                hdr.chunk_count, expected_count as u16,
                "data_size={data_size} chunk[{i}]: chunk_count field wrong"
            );
            assert_eq!(
                hdr.chunk_index, i as u16,
                "data_size={data_size} chunk[{i}]: chunk_index wrong"
            );
            if i == chunks.len() - 1 {
                assert_eq!(
                    frag.len(), expected_last_frag,
                    "data_size={data_size}: last fragment size mismatch"
                );
            } else {
                assert_eq!(
                    frag.len(), CHUNK_FRAGMENT_MAX,
                    "data_size={data_size} chunk[{i}]: non-last fragment should be CHUNK_FRAGMENT_MAX"
                );
            }
        }
    }

    #[test]
    fn chunk_boundary_empty() {
        // Empty data → 1 chunk, empty fragment
        let chunks = chunk_frame(0, false, 0, &[]);
        assert_eq!(chunks.len(), 1, "empty data must produce 1 chunk");
        let (hdr, frag) = parse_chunk_header(&chunks[0]).expect("parse failed");
        assert_eq!(hdr.chunk_count, 1);
        assert_eq!(hdr.chunk_index, 0);
        assert_eq!(frag.len(), 0);
    }

    #[test]
    fn chunk_boundary_size_1() {
        assert_chunk_boundaries(1, 1, 1);
    }

    #[test]
    fn chunk_boundary_size_1182() {
        assert_chunk_boundaries(CHUNK_FRAGMENT_MAX, 1, CHUNK_FRAGMENT_MAX);
    }

    #[test]
    fn chunk_boundary_size_1183() {
        assert_chunk_boundaries(CHUNK_FRAGMENT_MAX + 1, 2, 1);
    }

    #[test]
    fn chunk_boundary_size_2x1182() {
        assert_chunk_boundaries(2 * CHUNK_FRAGMENT_MAX, 2, CHUNK_FRAGMENT_MAX);
    }

    #[test]
    fn chunk_boundary_size_2x1182_plus_1() {
        assert_chunk_boundaries(2 * CHUNK_FRAGMENT_MAX + 1, 3, 1);
    }

    // ── Real FecEncoder repair roundtrip ──────────────────────────────────

    #[test]
    fn real_encoder_repair_roundtrip() {
        let config = FecConfig::default_streaming();
        let mut encoder = FecEncoder::new(config);
        // Push enough sources to trigger a repair (1/8 ratio → repair after 8 source)
        let mut repairs = vec![];
        for i in 0u32..8 {
            let payload = vec![i as u8; 20];
            let out = encoder.push_source(i, &payload);
            for r in out.repairs {
                repairs.push(r);
            }
        }
        assert!(!repairs.is_empty(), "expected at least one repair from 1/8 ratio over 8 sources");
        for repair in &repairs {
            let wire = encode_symbol_msg(repair);
            let parsed = parse_symbol_msg(&wire).expect("repair parse failed");
            assert_eq!(*repair, parsed, "repair symbol roundtrip mismatch");
        }
    }

    // ── parse_chunk_header: too-short → None ─────────────────────────────

    #[test]
    fn parse_chunk_header_too_short_is_none() {
        assert!(parse_chunk_header(&[0u8; 12]).is_none());
    }

    #[test]
    fn parse_chunk_header_exact_header_ok() {
        let (hdr, frag) = parse_chunk_header(&[0u8; 13]).expect("should succeed");
        assert_eq!(frag.len(), 0);
        assert_eq!(hdr.frame_id, 0);
    }

    // ── parse_ack_msg domain ──────────────────────────────────────────────

    #[test]
    fn ack_parse_empty_is_none() {
        assert!(parse_ack_msg(&[]).is_none());
    }

    #[test]
    fn ack_parse_needs_idr() {
        assert_eq!(parse_ack_msg(&[0x00]), Some(AckMsg::NeedsIdr));
    }

    #[test]
    fn ack_parse_subscribe() {
        assert_eq!(parse_ack_msg(&[0x01]), Some(AckMsg::Subscribe));
    }

    #[test]
    fn ack_parse_unknown_byte_is_none() {
        assert!(parse_ack_msg(&[0x02]).is_none());
    }

    #[test]
    fn ack_parse_4_bytes() {
        let seq: u32 = 0xDEAD_BEEF;
        let wire = seq.to_le_bytes();
        assert_eq!(parse_ack_msg(&wire), Some(AckMsg::Ack(seq)));
    }

    #[test]
    fn ack_parse_5_bytes_is_none() {
        assert!(parse_ack_msg(&[0x00, 0x00, 0x00, 0x00, 0x00]).is_none());
    }

    #[test]
    fn ack_parse_3_bytes_is_none() {
        assert!(parse_ack_msg(&[0x00, 0x00, 0x00]).is_none());
    }

    // ── AckMsg encode roundtrip ───────────────────────────────────────────

    #[test]
    fn ack_encode_roundtrip_subscribe() {
        let msg = AckMsg::Subscribe;
        let wire = encode_ack_msg(&msg);
        assert_eq!(parse_ack_msg(&wire), Some(msg));
    }

    #[test]
    fn ack_encode_roundtrip_needs_idr() {
        let msg = AckMsg::NeedsIdr;
        let wire = encode_ack_msg(&msg);
        assert_eq!(parse_ack_msg(&wire), Some(msg));
    }

    #[test]
    fn ack_encode_roundtrip_ack() {
        let msg = AckMsg::Ack(12345);
        let wire = encode_ack_msg(&msg);
        assert_eq!(parse_ack_msg(&wire), Some(msg));
    }
}

// ── Cross-language vector tests ────────────────────────────────────────────
//
// Deterministic fixture: Rust encoder generates messages from LCG-seeded frames;
// committed JSON is pinned here with include_str! and also consumed by the TS
// mirror test in tests/fec_cross_vectors.test.mjs.

#[cfg(test)]
mod cross_vector_tests {
    use super::{chunk_frame, encode_symbol_msg, parse_chunk_header, parse_symbol_msg};
    use crate::fec::{DecoderEvent, FecConfig, FecDecoder, FecEncoder};

    /// Dropped message indices: source seq 3 (frame 1 last chunk).
    /// Repair 0 covers seqs 0..4 — one missing → GE solves exactly.
    const DROPPED: &[usize] = &[3];

    // ── LCG helpers ───────────────────────────────────────────────────────

    fn lcg_byte(state: &mut u32) -> u8 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*state >> 24) as u8
    }

    fn gen_frame(size: usize, state: &mut u32) -> Vec<u8> {
        (0..size).map(|_| lcg_byte(state)).collect()
    }

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|v| format!("{v:02x}")).collect()
    }

    // ── Scenario definition ───────────────────────────────────────────────

    fn cross_config() -> FecConfig {
        FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 4,
            window_max_symbols: 32,
            window_max_bytes: 262_144,
        }
    }

    struct FrameSpec {
        id: u32,
        size: usize,
        key: bool,
        ts: u32,
    }

    fn frame_specs() -> Vec<FrameSpec> {
        vec![
            FrameSpec { id: 0, size: 500,  key: true,  ts: 1_000  },
            FrameSpec { id: 1, size: 2500, key: false, ts: 17_666 },
            FrameSpec { id: 2, size: 1183, key: false, ts: 34_333 },
            FrameSpec { id: 3, size: 0,    key: false, ts: 51_000 },
        ]
    }

    // ── Encoder side ──────────────────────────────────────────────────────

    /// Generate all encoder messages (source then repairs in emission order)
    /// and the raw frame data bytes per frame.
    fn gen_messages_and_frames() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        const SEED: u32 = 0x00C0_FFEE;
        let mut state = SEED;
        let specs = frame_specs();
        let frame_data: Vec<Vec<u8>> =
            specs.iter().map(|sp| gen_frame(sp.size, &mut state)).collect();

        let mut enc = FecEncoder::new(cross_config());
        let mut messages: Vec<Vec<u8>> = Vec::new();
        let mut seq: u32 = 0;

        for (i, sp) in specs.iter().enumerate() {
            let chunks = chunk_frame(sp.id, sp.key, sp.ts, &frame_data[i]);
            for chunk in &chunks {
                let out = enc.push_source(seq, chunk);
                messages.push(encode_symbol_msg(&out.source));
                for r in &out.repairs {
                    messages.push(encode_symbol_msg(r));
                }
                seq += 1;
            }
        }

        (messages, frame_data)
    }

    // ── Decoder verification ──────────────────────────────────────────────

    /// Feed all non-dropped messages to a fresh FecDecoder, reassemble frames,
    /// assert byte-equality with originals, return expected_frames JSON values.
    fn verify_decoder_and_expected(
        messages: &[Vec<u8>],
        frame_data: &[Vec<u8>],
    ) -> Vec<serde_json::Value> {
        let cfg = cross_config();
        let mut dec = FecDecoder::new(cfg.window_max_symbols, cfg.window_max_bytes);

        // frame_id → Vec<(chunk_index, fragment_bytes)>
        let mut chunks: std::collections::BTreeMap<u32, Vec<(u16, Vec<u8>)>> =
            std::collections::BTreeMap::new();

        for (i, msg) in messages.iter().enumerate() {
            if DROPPED.contains(&i) {
                continue;
            }
            let sym = parse_symbol_msg(msg).expect("all test messages must be valid");
            for ev in dec.push_symbol(sym) {
                if let DecoderEvent::Recovered { payload, .. } = ev {
                    if let Some((hdr, frag)) = parse_chunk_header(&payload) {
                        chunks
                            .entry(hdr.frame_id)
                            .or_default()
                            .push((hdr.chunk_index, frag.to_vec()));
                    }
                }
            }
        }

        let specs = frame_specs();
        specs.iter().zip(frame_data.iter()).map(|(sp, orig)| {
            let cvec = chunks
                .get(&sp.id)
                .unwrap_or_else(|| panic!("frame {} not recovered", sp.id));
            let mut sorted = cvec.clone();
            sorted.sort_by_key(|(idx, _)| *idx);
            let reassembled: Vec<u8> =
                sorted.into_iter().flat_map(|(_, f)| f).collect();
            assert_eq!(&reassembled, orig, "frame {} byte mismatch after FEC recovery", sp.id);

            serde_json::json!({
                "data_hex":    to_hex(orig),
                "frame_id":    sp.id,
                "frame_type":  if sp.key { "key" } else { "delta" },
                "timestamp_us": sp.ts,
            })
        }).collect()
    }

    // ── Fixture builder ───────────────────────────────────────────────────

    fn build_fixture_json() -> String {
        let (messages, frame_data) = gen_messages_and_frames();
        let expected_frames = verify_decoder_and_expected(&messages, &frame_data);
        let specs = frame_specs();

        let v = serde_json::json!({
            "dropped_message_indices": DROPPED,
            "expected_frames": expected_frames,
            "messages": messages.iter().map(|m| to_hex(m)).collect::<Vec<_>>(),
            "meta": {
                "config": {
                    "redundancy_denominator": 4u8,
                    "redundancy_numerator":   1u8,
                    "window_max_bytes":       262_144u32,
                    "window_max_symbols":     32u16,
                },
                "frames": specs.iter().map(|sp| serde_json::json!({
                    "frame_id":   sp.id,
                    "frame_type": if sp.key { "key" } else { "delta" },
                    "size":       sp.size,
                    "timestamp_us": sp.ts,
                })).collect::<Vec<_>>(),
                "lcg": {
                    "increment":   1_013_904_223u32,
                    "multiplier":  1_664_525u32,
                    "output":      "state >> 24",
                    "seed":        "0x00c0ffee",
                    "state_bits":  32u32,
                },
            },
        });

        serde_json::to_string_pretty(&v).expect("fixture serialisation must not fail")
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    /// Re-generate the fixture in memory and write it to tests/fixtures/fec_vectors.json.
    /// Run once with: cargo test -p transport-core write_fec_cross_vectors_fixture -- --ignored
    /// then commit the generated file.
    #[test]
    #[ignore]
    fn write_fec_cross_vectors_fixture() {
        let json = build_fixture_json();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("transport-core/ must have a parent (repo root)")
            .join("tests/fixtures/fec_vectors.json");
        std::fs::create_dir_all(path.parent().expect("fixtures/ dir")).expect("mkdir fixtures");
        std::fs::write(&path, &json).expect("write fec_vectors.json");
        println!(
            "wrote {} bytes ({} messages) to {}",
            json.len(),
            serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| v["messages"].as_array().map(|a| a.len()))
                .unwrap_or(0),
            path.display()
        );
    }

    /// Pinning test: regenerate the fixture in memory and assert it equals the
    /// file committed at tests/fixtures/fec_vectors.json (include_str! at compile
    /// time).  Fails immediately if the generator changes without re-committing.
    ///
    /// include_str! path: from transport-core/src/fec_wire.rs,
    /// two levels up reaches repo root, then tests/fixtures/fec_vectors.json.
    #[test]
    fn cross_vectors_match_committed_fixture() {
        let generated = build_fixture_json();
        let committed = include_str!("../../tests/fixtures/fec_vectors.json");
        assert_eq!(
            generated, committed,
            "regenerated fixture does not match committed tests/fixtures/fec_vectors.json; \
             re-run `cargo test -p transport-core write_fec_cross_vectors_fixture -- --ignored` and commit"
        );
    }
}
