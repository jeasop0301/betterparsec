//! Pure (no async, no I/O) wire/framing module for the `video_fec` and
//! `video_fec_ack` DataChannels.
//!
//! All integers are little-endian.  See `docs/design/fec-framing.md §2`.

use crate::fec;

// ── Constants ─────────────────────────────────────────────────────────────

/// Maximum on-wire size for a v2 source-symbol message.
pub const FEC_MSG_MAX: usize = 1200;
/// Maximum v2 source payload (including the v2 chunk header).
pub const V2_SOURCE_PAYLOAD_MAX: usize = 1185;
/// Maximum v2 repair payload.
pub const V2_REPAIR_PAYLOAD_MAX: usize = 1200;
/// Maximum on-wire size for a v2 repair-symbol message.
pub const V2_REPAIR_MSG_MAX: usize = 1221;
/// Maximum encoded frame size accepted by the v2 chunk layer.
pub const ENCODED_FRAME_MAX: usize = 4 * 1024 * 1024;
/// Maximum number of chunks in a v2 frame.
pub const V2_CHUNK_COUNT_MAX: u16 = 4096;

/// v1 chunk header length. Kept unchanged for degraded v1 compatibility.
pub const CHUNK_HEADER_LEN: usize = 13;
/// v1 fragment limit. Kept unchanged for degraded v1 compatibility.
pub const CHUNK_FRAGMENT_MAX: usize = 1182;
/// v2 chunk header length.
pub const CHUNK_V2_HEADER_LEN: usize = 21;
/// Maximum v2 frame fragment carried by one source symbol.
pub const CHUNK_V2_FRAGMENT_MAX: usize = 1164;

const V2_SOURCE_HEADER_LEN: usize = 15;
const V2_REPAIR_HEADER_LEN: usize = 21;

/// Sender-owned, nonzero stream epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Epoch(u32);

impl Epoch {
    pub fn new(value: u32) -> Option<Self> {
        (value != 0).then_some(Self(value))
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireVersion {
    V1,
    V2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FecWireError {
    Empty,
    UnknownKind(u8),
    Truncated,
    Oversize,
    InvalidEpoch,
    InvalidLength,
    CrcMismatch,
    InvalidChunkCount,
    InvalidChunkIndex,
    InvalidFrameType,
    InvalidEncodedFrameLength,
}

/// v2 symbols contain an epoch, making stream discontinuities explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V2Symbol {
    Source {
        epoch: Epoch,
        seq: u32,
        payload: Vec<u8>,
    },
    Repair {
        epoch: Epoch,
        repair_seq: u16,
        window_base: u32,
        window_end: u32,
        payload: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSymbolMsg {
    V1(fec::Symbol),
    V2(V2Symbol),
}

impl ParsedSymbolMsg {
    pub const fn version(&self) -> WireVersion {
        match self {
            Self::V1(_) => WireVersion::V1,
            Self::V2(_) => WireVersion::V2,
        }
    }
}

/// Parsed v2 chunk header. `encoded_frame_len` and `encoded_frame_crc32` apply
/// to the complete frame, not merely this fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkV2Header {
    pub frame_id: u32,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub frame_type_key: bool,
    pub timestamp_us: u32,
    pub encoded_frame_len: u32,
    pub encoded_frame_crc32: u32,
}

fn ieee_crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

// ── v1 chunk layer (unchanged) ─────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkHeader {
    pub frame_id: u32,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub frame_type_key: bool,
    pub timestamp_us: u32,
}

pub fn chunk_frame(
    frame_id: u32,
    frame_type_key: bool,
    timestamp_us: u32,
    data: &[u8],
) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return vec![encode_chunk(
            frame_id,
            0,
            1,
            frame_type_key,
            timestamp_us,
            &[],
        )];
    }

    let chunk_count = data.len().div_ceil(CHUNK_FRAGMENT_MAX);
    debug_assert!(
        chunk_count <= u16::MAX as usize,
        "chunk_count overflows u16"
    );
    data.chunks(CHUNK_FRAGMENT_MAX)
        .enumerate()
        .map(|(i, fragment)| {
            encode_chunk(
                frame_id,
                i as u16,
                chunk_count as u16,
                frame_type_key,
                timestamp_us,
                fragment,
            )
        })
        .collect()
}

fn encode_chunk(
    frame_id: u32,
    chunk_index: u16,
    chunk_count: u16,
    frame_type_key: bool,
    timestamp_us: u32,
    fragment: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(CHUNK_HEADER_LEN + fragment.len());
    buf.extend_from_slice(&frame_id.to_le_bytes());
    buf.extend_from_slice(&chunk_index.to_le_bytes());
    buf.extend_from_slice(&chunk_count.to_le_bytes());
    buf.push(u8::from(frame_type_key));
    buf.extend_from_slice(&timestamp_us.to_le_bytes());
    buf.extend_from_slice(fragment);
    buf
}

pub fn parse_chunk_header(buf: &[u8]) -> Option<(ChunkHeader, &[u8])> {
    if buf.len() < CHUNK_HEADER_LEN {
        return None;
    }
    Some((
        ChunkHeader {
            frame_id: u32::from_le_bytes(buf[0..4].try_into().ok()?),
            chunk_index: u16::from_le_bytes(buf[4..6].try_into().ok()?),
            chunk_count: u16::from_le_bytes(buf[6..8].try_into().ok()?),
            frame_type_key: buf[8] != 0,
            timestamp_us: u32::from_le_bytes(buf[9..13].try_into().ok()?),
        },
        &buf[CHUNK_HEADER_LEN..],
    ))
}

// ── v2 chunk layer ─────────────────────────────────────────────────────────

/// Chunk an encoded frame using the v2, integrity-checked header.
pub fn chunk_frame_v2(
    frame_id: u32,
    frame_type_key: bool,
    timestamp_us: u32,
    data: &[u8],
) -> Result<Vec<Vec<u8>>, FecWireError> {
    if data.len() > ENCODED_FRAME_MAX {
        return Err(FecWireError::InvalidEncodedFrameLength);
    }
    let chunk_count = data.len().max(1).div_ceil(CHUNK_V2_FRAGMENT_MAX);
    if chunk_count > usize::from(V2_CHUNK_COUNT_MAX) {
        return Err(FecWireError::InvalidChunkCount);
    }
    let crc = ieee_crc32(data);
    let fragments: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(CHUNK_V2_FRAGMENT_MAX).collect()
    };
    Ok(fragments
        .into_iter()
        .enumerate()
        .map(|(index, fragment)| {
            encode_chunk_v2(
                &ChunkV2Header {
                    frame_id,
                    chunk_index: index as u16,
                    chunk_count: chunk_count as u16,
                    frame_type_key,
                    timestamp_us,
                    encoded_frame_len: data.len() as u32,
                    encoded_frame_crc32: crc,
                },
                fragment,
            )
            .expect("chunk_frame_v2 generated a valid chunk")
        })
        .collect())
}

/// Encode one already-validated v2 chunk.
pub fn encode_chunk_v2(header: &ChunkV2Header, fragment: &[u8]) -> Result<Vec<u8>, FecWireError> {
    validate_chunk_v2(header, fragment)?;
    let mut buf = Vec::with_capacity(CHUNK_V2_HEADER_LEN + fragment.len());
    buf.extend_from_slice(&header.frame_id.to_le_bytes());
    buf.extend_from_slice(&header.chunk_index.to_le_bytes());
    buf.extend_from_slice(&header.chunk_count.to_le_bytes());
    buf.push(u8::from(header.frame_type_key));
    buf.extend_from_slice(&header.timestamp_us.to_le_bytes());
    buf.extend_from_slice(&header.encoded_frame_len.to_le_bytes());
    buf.extend_from_slice(&header.encoded_frame_crc32.to_le_bytes());
    buf.extend_from_slice(fragment);
    Ok(buf)
}

/// Parse a v2 chunk without allocating. Complete-frame CRC validation happens
/// after reassembly, using [`validate_encoded_frame_v2`].
pub fn parse_chunk_v2(buf: &[u8]) -> Result<(ChunkV2Header, &[u8]), FecWireError> {
    if buf.len() < CHUNK_V2_HEADER_LEN {
        return Err(FecWireError::Truncated);
    }
    let frame_type = buf[8];
    let header = ChunkV2Header {
        frame_id: u32::from_le_bytes(buf[0..4].try_into().map_err(|_| FecWireError::Truncated)?),
        chunk_index: u16::from_le_bytes(buf[4..6].try_into().map_err(|_| FecWireError::Truncated)?),
        chunk_count: u16::from_le_bytes(buf[6..8].try_into().map_err(|_| FecWireError::Truncated)?),
        frame_type_key: frame_type == 1,
        timestamp_us: u32::from_le_bytes(
            buf[9..13].try_into().map_err(|_| FecWireError::Truncated)?,
        ),
        encoded_frame_len: u32::from_le_bytes(
            buf[13..17]
                .try_into()
                .map_err(|_| FecWireError::Truncated)?,
        ),
        encoded_frame_crc32: u32::from_le_bytes(
            buf[17..21]
                .try_into()
                .map_err(|_| FecWireError::Truncated)?,
        ),
    };
    if frame_type > 1 {
        return Err(FecWireError::InvalidFrameType);
    }
    validate_chunk_v2(&header, &buf[CHUNK_V2_HEADER_LEN..])?;
    Ok((header, &buf[CHUNK_V2_HEADER_LEN..]))
}

fn validate_chunk_v2(header: &ChunkV2Header, fragment: &[u8]) -> Result<(), FecWireError> {
    if header.chunk_count == 0 || header.chunk_count > V2_CHUNK_COUNT_MAX {
        return Err(FecWireError::InvalidChunkCount);
    }
    if header.chunk_index >= header.chunk_count {
        return Err(FecWireError::InvalidChunkIndex);
    }
    if usize::try_from(header.encoded_frame_len).unwrap_or(usize::MAX) > ENCODED_FRAME_MAX {
        return Err(FecWireError::InvalidEncodedFrameLength);
    }
    if fragment.len() > CHUNK_V2_FRAGMENT_MAX {
        return Err(FecWireError::Oversize);
    }
    Ok(())
}

pub fn validate_encoded_frame_v2(
    header: &ChunkV2Header,
    encoded_frame: &[u8],
) -> Result<(), FecWireError> {
    if encoded_frame.len() != header.encoded_frame_len as usize {
        return Err(FecWireError::InvalidLength);
    }
    if ieee_crc32(encoded_frame) != header.encoded_frame_crc32 {
        return Err(FecWireError::CrcMismatch);
    }
    Ok(())
}

// ── Symbol messages ───────────────────────────────────────────────────────

/// Serialise a v1 [`fec::Symbol`] unchanged.
pub fn encode_symbol_msg(sym: &fec::Symbol) -> Vec<u8> {
    match sym {
        fec::Symbol::Source { seq, payload } => {
            let mut buf = Vec::with_capacity(5 + payload.len());
            buf.push(0);
            buf.extend_from_slice(&seq.to_le_bytes());
            buf.extend_from_slice(payload);
            buf
        }
        fec::Symbol::Repair {
            repair_seq,
            window_base,
            window_end,
            payload,
        } => {
            let mut buf = Vec::with_capacity(11 + payload.len());
            buf.push(1);
            buf.extend_from_slice(&repair_seq.to_le_bytes());
            buf.extend_from_slice(&window_base.to_le_bytes());
            buf.extend_from_slice(&window_end.to_le_bytes());
            buf.extend_from_slice(payload);
            buf
        }
    }
}

/// Parse v1 messages only; v2 is deliberately not reinterpreted as v1.
pub fn parse_symbol_msg(buf: &[u8]) -> Option<fec::Symbol> {
    match parse_symbol_msg_versioned(buf).ok()? {
        ParsedSymbolMsg::V1(symbol) => Some(symbol),
        ParsedSymbolMsg::V2(_) => None,
    }
}

pub fn encode_symbol_msg_v2(sym: &V2Symbol) -> Result<Vec<u8>, FecWireError> {
    let (kind, epoch, repair_seq, window_base, window_end, seq, payload) = match sym {
        V2Symbol::Source {
            epoch,
            seq,
            payload,
        } => (2, *epoch, 0, 0, 0, *seq, payload.as_slice()),
        V2Symbol::Repair {
            epoch,
            repair_seq,
            window_base,
            window_end,
            payload,
        } => (
            3,
            *epoch,
            *repair_seq,
            *window_base,
            *window_end,
            0,
            payload.as_slice(),
        ),
    };
    let max = if kind == 2 {
        V2_SOURCE_PAYLOAD_MAX
    } else {
        V2_REPAIR_PAYLOAD_MAX
    };
    if payload.len() > max {
        return Err(FecWireError::Oversize);
    }
    let mut buf = Vec::with_capacity(
        if kind == 2 {
            V2_SOURCE_HEADER_LEN
        } else {
            V2_REPAIR_HEADER_LEN
        } + payload.len(),
    );
    buf.push(kind);
    buf.extend_from_slice(&epoch.get().to_le_bytes());
    if kind == 2 {
        buf.extend_from_slice(&seq.to_le_bytes());
    } else {
        buf.extend_from_slice(&repair_seq.to_le_bytes());
        buf.extend_from_slice(&window_base.to_le_bytes());
        buf.extend_from_slice(&window_end.to_le_bytes());
    }
    buf.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    buf.extend_from_slice(&ieee_crc32(payload).to_le_bytes());
    buf.extend_from_slice(payload);
    Ok(buf)
}

/// Parse a v1 or v2 symbol. Selected v2 bytes always produce a v2 result or
/// error; malformed v2 data never falls back to the v1 parser.
pub fn parse_symbol_msg_versioned(buf: &[u8]) -> Result<ParsedSymbolMsg, FecWireError> {
    let kind = *buf.first().ok_or(FecWireError::Empty)?;
    match kind {
        0 => {
            if buf.len() < 5 {
                return Err(FecWireError::Truncated);
            }
            Ok(ParsedSymbolMsg::V1(fec::Symbol::Source {
                seq: u32::from_le_bytes(buf[1..5].try_into().map_err(|_| FecWireError::Truncated)?),
                payload: buf[5..].to_vec(),
            }))
        }
        1 => {
            if buf.len() < 11 {
                return Err(FecWireError::Truncated);
            }
            Ok(ParsedSymbolMsg::V1(fec::Symbol::Repair {
                repair_seq: u16::from_le_bytes(
                    buf[1..3].try_into().map_err(|_| FecWireError::Truncated)?,
                ),
                window_base: u32::from_le_bytes(
                    buf[3..7].try_into().map_err(|_| FecWireError::Truncated)?,
                ),
                window_end: u32::from_le_bytes(
                    buf[7..11].try_into().map_err(|_| FecWireError::Truncated)?,
                ),
                payload: buf[11..].to_vec(),
            }))
        }
        2 => parse_v2_source(buf).map(ParsedSymbolMsg::V2),
        3 => parse_v2_repair(buf).map(ParsedSymbolMsg::V2),
        other => Err(FecWireError::UnknownKind(other)),
    }
}

fn parse_v2_source(buf: &[u8]) -> Result<V2Symbol, FecWireError> {
    if buf.len() < V2_SOURCE_HEADER_LEN {
        return Err(FecWireError::Truncated);
    }
    if buf.len() > FEC_MSG_MAX {
        return Err(FecWireError::Oversize);
    }
    let epoch = Epoch::new(u32::from_le_bytes(
        buf[1..5].try_into().map_err(|_| FecWireError::Truncated)?,
    ))
    .ok_or(FecWireError::InvalidEpoch)?;
    let seq = u32::from_le_bytes(buf[5..9].try_into().map_err(|_| FecWireError::Truncated)?);
    let len =
        u16::from_le_bytes(buf[9..11].try_into().map_err(|_| FecWireError::Truncated)?) as usize;
    if len > V2_SOURCE_PAYLOAD_MAX || buf.len() != V2_SOURCE_HEADER_LEN + len {
        return Err(FecWireError::InvalidLength);
    }
    let payload = &buf[V2_SOURCE_HEADER_LEN..];
    if ieee_crc32(payload)
        != u32::from_le_bytes(
            buf[11..15]
                .try_into()
                .map_err(|_| FecWireError::Truncated)?,
        )
    {
        return Err(FecWireError::CrcMismatch);
    }
    Ok(V2Symbol::Source {
        epoch,
        seq,
        payload: payload.to_vec(),
    })
}

fn parse_v2_repair(buf: &[u8]) -> Result<V2Symbol, FecWireError> {
    if buf.len() < V2_REPAIR_HEADER_LEN {
        return Err(FecWireError::Truncated);
    }
    if buf.len() > V2_REPAIR_MSG_MAX {
        return Err(FecWireError::Oversize);
    }
    let epoch = Epoch::new(u32::from_le_bytes(
        buf[1..5].try_into().map_err(|_| FecWireError::Truncated)?,
    ))
    .ok_or(FecWireError::InvalidEpoch)?;
    let repair_seq = u16::from_le_bytes(buf[5..7].try_into().map_err(|_| FecWireError::Truncated)?);
    let window_base =
        u32::from_le_bytes(buf[7..11].try_into().map_err(|_| FecWireError::Truncated)?);
    let window_end = u32::from_le_bytes(
        buf[11..15]
            .try_into()
            .map_err(|_| FecWireError::Truncated)?,
    );
    let len = u16::from_le_bytes(
        buf[15..17]
            .try_into()
            .map_err(|_| FecWireError::Truncated)?,
    ) as usize;
    if len > V2_REPAIR_PAYLOAD_MAX || buf.len() != V2_REPAIR_HEADER_LEN + len {
        return Err(FecWireError::InvalidLength);
    }
    let payload = &buf[V2_REPAIR_HEADER_LEN..];
    if ieee_crc32(payload)
        != u32::from_le_bytes(
            buf[17..21]
                .try_into()
                .map_err(|_| FecWireError::Truncated)?,
        )
    {
        return Err(FecWireError::CrcMismatch);
    }
    Ok(V2Symbol::Repair {
        epoch,
        repair_seq,
        window_base,
        window_end,
        payload: payload.to_vec(),
    })
}

// ── Control messages ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckMsg {
    Subscribe,
    NeedsIdr,
    Ack(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V2ControlMsg {
    Subscribe { epoch: Epoch },
    Ack { epoch: Epoch, highest_seq: u32 },
    NeedsIdr { epoch: Epoch, reason: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedControlMsg {
    V1(AckMsg),
    V2(V2ControlMsg),
}

impl ParsedControlMsg {
    pub const fn version(&self) -> WireVersion {
        match self {
            Self::V1(_) => WireVersion::V1,
            Self::V2(_) => WireVersion::V2,
        }
    }
}

pub fn parse_ack_msg(buf: &[u8]) -> Option<AckMsg> {
    match parse_control_msg(buf).ok()? {
        ParsedControlMsg::V1(msg) => Some(msg),
        ParsedControlMsg::V2(_) => None,
    }
}

pub fn encode_ack_msg(msg: &AckMsg) -> Vec<u8> {
    match msg {
        AckMsg::Subscribe => vec![0x01],
        AckMsg::NeedsIdr => vec![0x00],
        AckMsg::Ack(seq) => seq.to_le_bytes().to_vec(),
    }
}

pub fn encode_control_msg_v2(msg: &V2ControlMsg) -> Vec<u8> {
    match msg {
        V2ControlMsg::Subscribe { epoch } => {
            let mut out = vec![0x82];
            out.extend_from_slice(&epoch.get().to_le_bytes());
            out
        }
        V2ControlMsg::Ack { epoch, highest_seq } => {
            let mut out = vec![0x81];
            out.extend_from_slice(&epoch.get().to_le_bytes());
            out.extend_from_slice(&highest_seq.to_le_bytes());
            out
        }
        V2ControlMsg::NeedsIdr { epoch, reason } => {
            let mut out = vec![0x80];
            out.extend_from_slice(&epoch.get().to_le_bytes());
            out.push(*reason);
            out
        }
    }
}

/// Parse a v1 or selected-v2 control. Reserved selected-v2 kinds never fall
/// back to the v1 ACK parser, even when their malformed shape is four bytes.
pub fn parse_control_msg(buf: &[u8]) -> Result<ParsedControlMsg, FecWireError> {
    match buf {
        [] => Err(FecWireError::Empty),
        [0x80, epoch @ ..] => {
            if epoch.len() < 5 {
                return Err(FecWireError::Truncated);
            }
            if epoch.len() > 5 {
                return Err(FecWireError::InvalidLength);
            }
            Ok(ParsedControlMsg::V2(V2ControlMsg::NeedsIdr {
                epoch: Epoch::new(u32::from_le_bytes(
                    epoch[..4].try_into().map_err(|_| FecWireError::Truncated)?,
                ))
                .ok_or(FecWireError::InvalidEpoch)?,
                reason: epoch[4],
            }))
        }
        [0x81, epoch @ ..] => {
            if epoch.len() < 8 {
                return Err(FecWireError::Truncated);
            }
            if epoch.len() > 8 {
                return Err(FecWireError::InvalidLength);
            }
            Ok(ParsedControlMsg::V2(V2ControlMsg::Ack {
                epoch: Epoch::new(u32::from_le_bytes(
                    epoch[..4].try_into().map_err(|_| FecWireError::Truncated)?,
                ))
                .ok_or(FecWireError::InvalidEpoch)?,
                highest_seq: u32::from_le_bytes(
                    epoch[4..].try_into().map_err(|_| FecWireError::Truncated)?,
                ),
            }))
        }
        [0x82, epoch @ ..] => {
            if epoch.len() < 4 {
                return Err(FecWireError::Truncated);
            }
            if epoch.len() > 4 {
                return Err(FecWireError::InvalidLength);
            }
            Ok(ParsedControlMsg::V2(V2ControlMsg::Subscribe {
                epoch: Epoch::new(u32::from_le_bytes(
                    epoch.try_into().map_err(|_| FecWireError::Truncated)?,
                ))
                .ok_or(FecWireError::InvalidEpoch)?,
            }))
        }
        [0x00] => Ok(ParsedControlMsg::V1(AckMsg::NeedsIdr)),
        [0x01] => Ok(ParsedControlMsg::V1(AckMsg::Subscribe)),
        [a, b, c, d] => Ok(ParsedControlMsg::V1(AckMsg::Ack(u32::from_le_bytes([
            *a, *b, *c, *d,
        ])))),
        [kind, ..] => Err(FecWireError::UnknownKind(*kind)),
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
        let sym = Symbol::Source {
            seq: 42,
            payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
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
        let sym = Symbol::Source {
            seq: 0,
            payload: vec![],
        };
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
        let sym = Symbol::Source {
            seq: 1,
            payload: vec![0x42, 0x43],
        };
        let wire = encode_symbol_msg(&sym);
        let expected: &[u8] = &[0x00, 0x01, 0x00, 0x00, 0x00, 0x42, 0x43];
        assert_eq!(
            wire.as_slice(),
            expected,
            "source symbol byte layout mismatch"
        );
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
            0x01, // kind=1
            0x02, 0x00, // repair_seq=2 LE
            0x00, 0x00, 0x00, 0x00, // window_base=0 LE
            0x01, 0x00, 0x00, 0x00, // window_end=1 LE
            0xAB, 0xCD, // payload
        ];
        assert_eq!(
            wire.as_slice(),
            expected,
            "repair symbol byte layout mismatch"
        );
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
        assert!(
            parse_symbol_msg(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
                .is_none()
        );
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
            0x01, 0x00, 0x00, 0x00, // frame_id=1 LE
            0x00, 0x00, // chunk_index=0 LE
            0x01, 0x00, // chunk_count=1 LE
            0x01, // frame_type=key
            0x44, 0x33, 0x22, 0x11, // timestamp_us LE
        ];
        assert_eq!(hdr, expected, "chunk header byte layout mismatch");
    }

    // ── Chunk boundary domain ─────────────────────────────────────────────

    fn assert_chunk_boundaries(data_size: usize, expected_count: usize, expected_last_frag: usize) {
        let data: Vec<u8> = (0..data_size).map(|i| i as u8).collect();
        let chunks = chunk_frame(0, false, 0, &data);
        assert_eq!(
            chunks.len(),
            expected_count,
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
                    frag.len(),
                    expected_last_frag,
                    "data_size={data_size}: last fragment size mismatch"
                );
            } else {
                assert_eq!(
                    frag.len(),
                    CHUNK_FRAGMENT_MAX,
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
        assert!(
            !repairs.is_empty(),
            "expected at least one repair from 1/8 ratio over 8 sources"
        );
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
    #[test]
    fn reserved_v2_control_4_byte_shapes_never_parse_as_v1_acks() {
        for kind in [0x80, 0x81, 0x82] {
            let wire = [kind, 0x11, 0x22, 0x33];
            assert_eq!(parse_control_msg(&wire), Err(FecWireError::Truncated));
            assert_eq!(parse_ack_msg(&wire), None);
        }
    }

    #[test]
    fn v2_controls_require_exact_lengths() {
        for (wire, message) in [
            (
                vec![0x80, 1, 0, 0, 0, 7],
                V2ControlMsg::NeedsIdr {
                    epoch: Epoch::new(1).expect("nonzero epoch must be valid"),
                    reason: 7,
                },
            ),
            (
                vec![0x81, 2, 0, 0, 0, 9, 0, 0, 0],
                V2ControlMsg::Ack {
                    epoch: Epoch::new(2).expect("nonzero epoch must be valid"),
                    highest_seq: 9,
                },
            ),
            (
                vec![0x82, 3, 0, 0, 0],
                V2ControlMsg::Subscribe {
                    epoch: Epoch::new(3).expect("nonzero epoch must be valid"),
                },
            ),
        ] {
            assert_eq!(parse_control_msg(&wire), Ok(ParsedControlMsg::V2(message)));
            assert_eq!(
                parse_control_msg(&wire[..wire.len() - 1]),
                Err(FecWireError::Truncated)
            );

            let mut long = wire;
            long.push(0);
            assert_eq!(parse_control_msg(&long), Err(FecWireError::InvalidLength));
        }
    }

    #[test]
    fn ordinary_v1_ack_with_nonreserved_first_byte_still_parses() {
        let wire = 0x1020_30efu32.to_le_bytes();
        assert_eq!(
            parse_control_msg(&wire),
            Ok(ParsedControlMsg::V1(AckMsg::Ack(0x1020_30ef)))
        );
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
    #[test]
    fn v2_source_golden_roundtrip() {
        let symbol = V2Symbol::Source {
            epoch: Epoch::new(0x1122_3344).expect("nonzero epoch must be valid"),
            seq: 0x5566_7788,
            payload: vec![0xaa, 0xbb],
        };
        let wire = encode_symbol_msg_v2(&symbol).expect("source symbol must encode");
        assert_eq!(
            wire,
            vec![
                0x02, 0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55, 0x02, 0x00, 0x98, 0x2c, 0x82,
                0x49, 0xaa, 0xbb,
            ]
        );
        assert_eq!(
            parse_symbol_msg_versioned(&wire),
            Ok(ParsedSymbolMsg::V2(symbol))
        );
    }

    #[test]
    fn v2_repair_golden_roundtrip() {
        let symbol = V2Symbol::Repair {
            epoch: Epoch::new(1).expect("nonzero epoch must be valid"),
            repair_seq: 2,
            window_base: 3,
            window_end: 4,
            payload: vec![0x42],
        };
        let wire = encode_symbol_msg_v2(&symbol).expect("repair symbol must encode");
        assert_eq!(
            wire,
            vec![
                0x03, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x03, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00,
                0x00, 0x01, 0x00, 0x31, 0xcf, 0xd0, 0x4a, 0x42,
            ]
        );
        assert_eq!(
            parse_symbol_msg_versioned(&wire),
            Ok(ParsedSymbolMsg::V2(symbol))
        );
    }

    #[test]
    fn v2_source_rejects_corruption_truncation_and_oversize() {
        let symbol = V2Symbol::Source {
            epoch: Epoch::new(1).expect("nonzero epoch must be valid"),
            seq: 0,
            payload: vec![7; 2],
        };
        let mut corrupted = encode_symbol_msg_v2(&symbol).expect("source symbol must encode");
        *corrupted
            .last_mut()
            .expect("encoded source symbol must contain a payload byte") ^= 1;
        assert_eq!(
            parse_symbol_msg_versioned(&corrupted),
            Err(FecWireError::CrcMismatch)
        );
        assert_eq!(
            parse_symbol_msg_versioned(&corrupted[..14]),
            Err(FecWireError::Truncated)
        );
        assert_eq!(
            encode_symbol_msg_v2(&V2Symbol::Source {
                epoch: Epoch::new(1).expect("nonzero epoch must be valid"),
                seq: 0,
                payload: vec![0; V2_SOURCE_PAYLOAD_MAX + 1],
            }),
            Err(FecWireError::Oversize)
        );
    }

    #[test]
    fn v2_chunk_golden_crc_and_bounds() {
        let chunks = chunk_frame_v2(1, true, 0x1122_3344, &[0xaa, 0xbb]).expect("frame must chunk");
        assert_eq!(
            chunks[0],
            vec![
                1, 0, 0, 0, 0, 0, 1, 0, 1, 0x44, 0x33, 0x22, 0x11, 2, 0, 0, 0, 0x98, 0x2c, 0x82,
                0x49, 0xaa, 0xbb,
            ]
        );
        let (header, fragment) = parse_chunk_v2(&chunks[0]).expect("encoded chunk must parse");
        assert_eq!(fragment, [0xaa, 0xbb]);
        assert_eq!(validate_encoded_frame_v2(&header, fragment), Ok(()));
        assert_eq!(
            parse_chunk_v2(&chunks[0][..20]),
            Err(FecWireError::Truncated)
        );
        assert_eq!(
            chunk_frame_v2(0, false, 0, &vec![0; ENCODED_FRAME_MAX + 1]),
            Err(FecWireError::InvalidEncodedFrameLength)
        );
    }

    #[test]
    fn v2_controls_are_versioned_and_validate_epoch() {
        let epoch = Epoch::new(9).expect("nonzero epoch must be valid");
        for message in [
            V2ControlMsg::Subscribe { epoch },
            V2ControlMsg::Ack {
                epoch,
                highest_seq: 7,
            },
            V2ControlMsg::NeedsIdr { epoch, reason: 3 },
        ] {
            let wire = encode_control_msg_v2(&message);
            assert_eq!(parse_control_msg(&wire), Ok(ParsedControlMsg::V2(message)));
            assert_eq!(parse_ack_msg(&wire), None);
        }
        assert_eq!(
            parse_control_msg(&[0x82, 0, 0, 0, 0]),
            Err(FecWireError::InvalidEpoch)
        );
    }
}

// ── Cross-language vector tests ────────────────────────────────────────────
//
// Deterministic fixture: Rust encoder generates messages from LCG-seeded frames;
// committed JSON is pinned here with include_str! and also consumed by the TS
// mirror test in tests/fec_cross_vectors.test.mjs.

#[cfg(test)]
mod cross_vector_tests {
    use super::{
        Epoch, V2Symbol, chunk_frame, encode_symbol_msg, encode_symbol_msg_v2, ieee_crc32,
        parse_chunk_header, parse_symbol_msg,
    };
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
            FrameSpec {
                id: 0,
                size: 500,
                key: true,
                ts: 1_000,
            },
            FrameSpec {
                id: 1,
                size: 2500,
                key: false,
                ts: 17_666,
            },
            FrameSpec {
                id: 2,
                size: 1183,
                key: false,
                ts: 34_333,
            },
            FrameSpec {
                id: 3,
                size: 0,
                key: false,
                ts: 51_000,
            },
        ]
    }

    // ── Encoder side ──────────────────────────────────────────────────────

    /// Generate all encoder messages (source then repairs in emission order)
    /// and the raw frame data bytes per frame.
    fn gen_messages_and_frames() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        const SEED: u32 = 0x00C0_FFEE;
        let mut state = SEED;
        let specs = frame_specs();
        let frame_data: Vec<Vec<u8>> = specs
            .iter()
            .map(|sp| gen_frame(sp.size, &mut state))
            .collect();

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
                if let DecoderEvent::Recovered { payload, .. } = ev
                    && let Some((hdr, frag)) = parse_chunk_header(&payload)
                {
                    chunks
                        .entry(hdr.frame_id)
                        .or_default()
                        .push((hdr.chunk_index, frag.to_vec()));
                }
            }
        }

        let specs = frame_specs();
        specs
            .iter()
            .zip(frame_data.iter())
            .map(|(sp, orig)| {
                let cvec = chunks
                    .get(&sp.id)
                    .unwrap_or_else(|| panic!("frame {} not recovered", sp.id));
                let mut sorted = cvec.clone();
                sorted.sort_by_key(|(idx, _)| *idx);
                let reassembled: Vec<u8> = sorted.into_iter().flat_map(|(_, f)| f).collect();
                assert_eq!(
                    &reassembled, orig,
                    "frame {} byte mismatch after FEC recovery",
                    sp.id
                );

                serde_json::json!({
                    "data_hex":    to_hex(orig),
                    "frame_id":    sp.id,
                    "frame_type":  if sp.key { "key" } else { "delta" },
                    "timestamp_us": sp.ts,
                })
            })
            .collect()
    }
    fn v2_fixture_records() -> Vec<serde_json::Value> {
        let symbols = vec![
            V2Symbol::Source {
                epoch: Epoch::new(0x0102_0304).expect("canonical epoch is nonzero"),
                seq: 0xa0b0_c0d0,
                payload: vec![
                    0x04, 0x03, 0x02, 0x01, 0, 0, 1, 0, 1, 8, 7, 6, 5, 1, 0, 0, 0, 0x7b, 0xa5, 1,
                    0xe4, 0xaa,
                ],
            },
            V2Symbol::Repair {
                epoch: Epoch::new(0x1122_3344).expect("canonical epoch is nonzero"),
                repair_seq: 0x5566,
                window_base: 0x7788_99aa,
                window_end: 0xddee_ff00,
                payload: vec![1, 2, 3],
            },
        ];

        symbols
            .into_iter()
            .map(|symbol| {
                let encoded = encode_symbol_msg_v2(&symbol).expect("canonical v2 symbol is valid");
                match symbol {
                    V2Symbol::Source {
                        epoch,
                        seq,
                        payload,
                    } => serde_json::json!({
                        "encoded_hex": to_hex(&encoded),
                        "epoch": epoch.get(),
                        "kind": "source",
                        "payload_crc32": ieee_crc32(&payload),
                        "payload_hex": to_hex(&payload),
                        "seq": seq,
                    }),
                    V2Symbol::Repair {
                        epoch,
                        repair_seq,
                        window_base,
                        window_end,
                        payload,
                    } => serde_json::json!({
                        "encoded_hex": to_hex(&encoded),
                        "epoch": epoch.get(),
                        "kind": "repair",
                        "payload_crc32": ieee_crc32(&payload),
                        "payload_hex": to_hex(&payload),
                        "repair_seq": repair_seq,
                        "window_base": window_base,
                        "window_end": window_end,
                    }),
                }
            })
            .collect()
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
            "v2_symbols": v2_fixture_records(),
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
        let committed_value: serde_json::Value =
            serde_json::from_str(committed).expect("committed fixture must be valid JSON");
        assert_eq!(
            committed_value["v2_symbols"],
            serde_json::Value::Array(v2_fixture_records()),
            "committed v2 symbols do not match the canonical Rust inputs"
        );
        assert_eq!(
            generated, committed,
            "regenerated fixture does not match committed tests/fixtures/fec_vectors.json; \
             re-run `cargo test -p transport-core write_fec_cross_vectors_fixture -- --ignored` and commit"
        );
    }
}
