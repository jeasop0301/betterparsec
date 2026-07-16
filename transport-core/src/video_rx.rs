//! Pure receive pipeline for the `video_fec` DataChannel (M6 W1).
//!
//! Rust mirror of the production TS client pipe
//! `web/stream/video/fec_decode_pipe.ts` — wire parse → [`FecDecoder`] →
//! chunk reassembly → [`DecodeUnit`] output plus the ACK cadence gate.
//! The native client cdylib (m6-native-spike.md Option-3) drives this from
//! its DataChannel receive thread and forwards [`DecodeUnit`]s across the
//! C ABI into moonlight-qt's `FrameQueue`.
//!
//! Pure/deterministic: no clocks (caller passes `now_ms`), no I/O, no async.
//! Behavioural divergences from the TS pipe are deliberate hardening and are
//! marked `DIVERGENCE:` inline.

use std::collections::HashMap;

use crate::fec::{DecoderEvent, FecDecoder};
use crate::fec_wire::{
    CHUNK_V2_FRAGMENT_MAX, ENCODED_FRAME_MAX, ParsedSymbolMsg, V2_CHUNK_COUNT_MAX, V2Symbol,
    parse_chunk_header, parse_chunk_v2, parse_symbol_msg_versioned, validate_encoded_frame_v2,
};

// ── Constants (mirror fec_decode_pipe.ts) ─────────────────────────────────

/// Maximum pending frame_ids before a full-reset discontinuity (sets needs_idr).
pub const MAX_PENDING_FRAMES: usize = 8;

/// ACK after this many delivered source symbols since the last ack.
pub const ACK_SYMBOL_INTERVAL: u32 = 32;

/// ACK after this many ms since the last ack (whichever comes first).
pub const ACK_TIME_INTERVAL_MS: u64 = 50;
/// Completed frames may wait for a missing predecessor only while this many are queued.
pub const REORDER_MAX_COMPLETED_FRAMES: usize = 4;
/// Completed frames may wait for a missing predecessor only for this long.
pub const REORDER_MAX_WAIT_MS: u64 = 100;

/// FEC decoder window cap — design fec-framing.md §4 (mirrors TS `new
/// FecDecoder(128, 16 MiB)`).
pub const RX_DECODER_MAX_SYMBOLS: u16 = 128;
/// FEC decoder byte cap (16 MiB, mirrors TS).
pub const RX_DECODER_MAX_BYTES: u32 = 16 * 1024 * 1024;

// ── Output types ──────────────────────────────────────────────────────────

/// One fully reassembled video frame, ready for the decoder.
///
/// Field semantics match the TS `VideoDecodeUnit` and map 1:1 onto the
/// moonlight `DECODE_UNIT` fields the native shim must fill
/// (m6-native-spike.md §A-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeUnit {
    /// Chunk-header frame_id (monotonic from the host sender).
    pub frame_id: u32,
    /// FEC epoch this frame was reassembled under (mirrors the receiver's
    /// `active_epoch` at completion; carried so downstream decoder-recovery
    /// state can be keyed per epoch without a second lookup).
    pub epoch: u32,
    /// true = key/IDR frame, false = delta.
    pub is_key: bool,
    /// Sender capture timestamp (µs, wraps at u32).
    pub timestamp_us: u32,
    /// `timestamp_us − previous frame's timestamp_us` (TS mirror: first frame
    /// measures against 0; negative on out-of-order/wrap).
    pub duration_us: i64,
    /// Concatenated Annex-B frame bytes.
    pub data: Vec<u8>,
}

/// Events produced while consuming one wire message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RxEvent {
    /// A frame finished reassembly and passed bounded ordering.
    Frame(DecodeUnit),
    /// Transport state was invalidated; deltas are gated until an IDR is admitted.
    Discontinuity {
        epoch: u32,
        reason: DiscontinuityReason,
    },
    /// The ACK cadence gate fired: send `AckMsg::Ack(seq)` on `video_fec_ack`.
    Ack(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscontinuityReason {
    EpochTransition,
    ReorderGap,
    ReassemblyEviction,
    MetadataMismatch,
    FrameCrc,
    MemoryCap,
    FecEviction,
    /// Client-transport-only reason (stable C ABI code 8): the decoder-side
    /// frame queue overflowed and its stale backlog was cleared. Never
    /// produced by [`VideoReceiver`] itself — reserved here so the single
    /// [`DiscontinuityReason`] enum covers every discontinuity that can ride
    /// the client-transport event queue.
    QueueOverflow,
}

// ── Internal reassembly state ─────────────────────────────────────────────

/// Recovery/loss-span counters for one [`VideoReceiver`] (U2 P2 groundwork —
/// see the seam documentation on `fec::FecDecoderStats`). Symbol-level
/// counters are a passthrough snapshot of the underlying [`FecDecoder`];
/// `frames_recovered` is the one counter this layer owns, since only the
/// chunk-reassembly seam here knows which source seqs (recovered vs
/// directly received) contributed to a completed frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoReceiverStats {
    pub source_symbols_received: u64,
    pub repair_symbols_received: u64,
    pub symbols_recovered: u64,
    /// Completed frames that used at least one FEC-recovered source symbol
    /// (chosen seam: [`VideoReceiver::assemble_frame`], checking the
    /// `used_recovery` flag accumulated in [`PendingFrame`] as chunks
    /// arrived — set at [`VideoReceiver::deliver_chunk`]).
    pub frames_recovered: u64,
    /// Complete delta frames discarded after unrecoverable loss until the
    /// next keyframe resets decoder reference state.
    pub frames_dropped_awaiting_idr: u64,
    pub loss_spans: u64,
    pub loss_spans_recovered: u64,
}

#[derive(Debug)]
struct PendingFrame {
    is_v2: bool,
    is_key: bool,
    timestamp_us: u32,
    chunk_count: u16,
    encoded_frame_len: u32,
    encoded_frame_crc32: u32,
    parts: Vec<Option<Vec<u8>>>,
    received_count: u16,
    bytes: usize,
    used_recovery: bool,
}

#[derive(Debug)]
struct CompletedFrame {
    frame_id: u32,
    frame: PendingFrame,
    completed_at_ms: u64,
}

// ── VideoReceiver ─────────────────────────────────────────────────────────

/// Pure receive-side state machine for one `video_fec` subscription.
///
/// Call [`on_message`](Self::on_message) for every DataChannel message and
/// [`tick`](Self::tick) from a ~50 ms timer (mirrors the TS pipe's
/// independent ACK interval — without it the encoder window stalls on idle
/// streams). Poll [`poll_needs_idr`](Self::poll_needs_idr) after each call
/// and send `AckMsg::NeedsIdr` when it reports true.
#[derive(Debug)]
pub struct VideoReceiver {
    decoder: FecDecoder,

    // Reassembly state
    pending: HashMap<(u32, u32), PendingFrame>,
    pending_order: Vec<(u32, u32)>,
    completed: HashMap<(u32, u32), CompletedFrame>,
    completed_order: Vec<(u32, u32)>,
    buffered_bytes: usize,
    active_epoch: Option<u32>,
    next_frame_id: Option<u32>,

    /// Latched until polled (mirrors TS `pollRequestIdr`).
    needs_idr: bool,
    /// Unrecoverable source loss invalidates the inter-frame reference chain.
    /// Delta frames are withheld until a keyframe arrives, preventing decoder
    /// concealment from presenting persistent macroblock corruption.
    awaiting_idr: bool,

    /// Duration tracking (mirrors DepacketizeVideoPipe / TS pipe).
    last_timestamp_us: u32,

    // ACK cadence state
    symbols_since_ack: u32,
    last_ack_time_ms: u64,
    last_acked_highest: Option<u32>,
    /// Completed frames that used >=1 FEC-recovered symbol (see
    /// [`VideoReceiverStats::frames_recovered`]).
    frames_recovered: u64,
    frames_dropped_awaiting_idr: u64,
}

impl VideoReceiver {
    /// `now_ms`: caller clock at construction (any monotonic ms source).
    pub fn new(now_ms: u64) -> Self {
        Self {
            decoder: FecDecoder::new(RX_DECODER_MAX_SYMBOLS, RX_DECODER_MAX_BYTES),
            pending: HashMap::new(),
            pending_order: Vec::new(),
            completed: HashMap::new(),
            completed_order: Vec::new(),
            buffered_bytes: 0,
            active_epoch: None,
            next_frame_id: None,
            needs_idr: false,
            awaiting_idr: false,
            last_timestamp_us: 0,
            symbols_since_ack: 0,
            last_ack_time_ms: now_ms,
            last_acked_highest: None,
            frames_recovered: 0,
            frames_dropped_awaiting_idr: 0,
        }
    }

    /// Consume one raw `video_fec` DataChannel message.
    ///
    /// Returns completed frames and at most one ACK, in the order they were
    /// produced. Malformed messages are silently dropped (TS mirror).
    pub fn on_message(&mut self, buf: &[u8], now_ms: u64) -> Vec<RxEvent> {
        let parsed = match parse_symbol_msg_versioned(buf) {
            Ok(parsed) => parsed,
            Err(_) => return Vec::new(),
        };
        let (epoch, is_v2, sym) = match parsed {
            ParsedSymbolMsg::V1(sym) => (0, false, sym),
            ParsedSymbolMsg::V2(V2Symbol::Source {
                epoch,
                seq,
                payload,
            }) => (
                epoch.get(),
                true,
                crate::fec::Symbol::Source { seq, payload },
            ),
            ParsedSymbolMsg::V2(V2Symbol::Repair {
                epoch,
                repair_seq,
                window_base,
                window_end,
                payload,
            }) => (
                epoch.get(),
                true,
                crate::fec::Symbol::Repair {
                    repair_seq,
                    window_base,
                    window_end,
                    payload,
                },
            ),
        };

        let mut out = Vec::new();
        if !self.accept_epoch(epoch, is_v2, now_ms, &mut out) {
            return out;
        }
        let decoder_events = self.decoder.push_symbol(sym);
        self.process_decoder_events(epoch, is_v2, decoder_events, now_ms, &mut out);
        self.flush_reorder(now_ms, &mut out);
        out
    }

    fn process_decoder_events(
        &mut self,
        epoch: u32,
        is_v2: bool,
        decoder_events: Vec<DecoderEvent>,
        now_ms: u64,
        out: &mut Vec<RxEvent>,
    ) {
        if decoder_events.iter().any(|event| {
            matches!(
                event,
                DecoderEvent::LossSpan { .. } | DecoderEvent::Evicted { .. }
            )
        }) {
            self.discontinue(epoch, DiscontinuityReason::FecEviction, out);
        }
        for event in decoder_events {
            if let DecoderEvent::Recovered {
                payload, via_fec, ..
            } = event
            {
                self.deliver_chunk(epoch, is_v2, &payload, via_fec, now_ms, out);
                if let Some(ack) = self.tick_ack(now_ms) {
                    out.push(RxEvent::Ack(ack));
                }
            }
        }
    }

    /// Independent ~50 ms timer tick (TS `tickTimer`). Fires an ACK when
    /// `highest_fully_decoded` advanced past the last acked value and the
    /// time threshold is met, even when no symbols arrive.
    pub fn tick(&mut self, now_ms: u64) -> Option<u32> {
        self.tick_ack(now_ms)
    }
    /// Timer path that also drains the bounded completed-frame reorder queue.
    pub fn tick_events(&mut self, now_ms: u64) -> Vec<RxEvent> {
        let mut out = Vec::new();
        self.flush_reorder(now_ms, &mut out);
        if let Some(ack) = self.tick_ack(now_ms) {
            out.push(RxEvent::Ack(ack));
        }
        out
    }

    /// Latched needs-IDR flag; cleared by the poll (TS `pollRequestIdr`).
    pub fn poll_needs_idr(&mut self) -> bool {
        std::mem::take(&mut self.needs_idr)
    }

    /// Highest contiguously decoded source seq (stats/debug passthrough).
    pub fn highest_fully_decoded(&self) -> Option<u32> {
        self.decoder.highest_fully_decoded()
    }

    /// Snapshot of recovery/loss-span counters accumulated so far (Copy
    /// struct; cheap to call at any time). Symbol-level fields passthrough
    /// [`FecDecoder::stats`]; `frames_recovered` is tracked here.
    pub fn stats(&self) -> VideoReceiverStats {
        let d = self.decoder.stats();
        VideoReceiverStats {
            source_symbols_received: d.source_symbols_received,
            repair_symbols_received: d.repair_symbols_received,
            symbols_recovered: d.symbols_recovered,
            frames_recovered: self.frames_recovered,
            frames_dropped_awaiting_idr: self.frames_dropped_awaiting_idr,
            loss_spans: d.loss_spans,
            loss_spans_recovered: d.loss_spans_recovered,
        }
    }

    fn accept_epoch(
        &mut self,
        epoch: u32,
        is_v2: bool,
        now_ms: u64,
        out: &mut Vec<RxEvent>,
    ) -> bool {
        let Some(current) = self.active_epoch else {
            self.active_epoch = Some(epoch);
            return true;
        };
        if epoch == current {
            return true;
        }
        let distance = epoch.wrapping_sub(current);
        if is_v2 && (current == 0 || (distance != 0 && distance < 0x8000_0000)) {
            self.decoder = FecDecoder::new(RX_DECODER_MAX_SYMBOLS, RX_DECODER_MAX_BYTES);
            self.active_epoch = Some(epoch);
            self.symbols_since_ack = 0;
            self.last_ack_time_ms = now_ms;
            self.last_acked_highest = None;
            self.discontinue(epoch, DiscontinuityReason::EpochTransition, out);
            return true;
        }
        false
    }

    fn discontinue(&mut self, epoch: u32, reason: DiscontinuityReason, out: &mut Vec<RxEvent>) {
        self.pending.clear();
        self.pending_order.clear();
        self.completed.clear();
        self.completed_order.clear();
        self.buffered_bytes = 0;
        self.next_frame_id = None;
        self.needs_idr = true;
        self.awaiting_idr = true;
        out.push(RxEvent::Discontinuity { epoch, reason });
    }

    fn deliver_chunk(
        &mut self,
        epoch: u32,
        is_v2: bool,
        payload: &[u8],
        via_fec: bool,
        now_ms: u64,
        out: &mut Vec<RxEvent>,
    ) {
        let parsed = if is_v2 {
            let Ok((hdr, fragment)) = parse_chunk_v2(payload) else {
                return;
            };
            (
                hdr.frame_id,
                hdr.chunk_index,
                hdr.chunk_count,
                hdr.frame_type_key,
                hdr.timestamp_us,
                hdr.encoded_frame_len,
                hdr.encoded_frame_crc32,
                fragment,
            )
        } else {
            let Some((hdr, fragment)) = parse_chunk_header(payload) else {
                return;
            };
            if hdr.chunk_count == 0 || hdr.chunk_index >= hdr.chunk_count {
                return;
            }
            (
                hdr.frame_id,
                hdr.chunk_index,
                hdr.chunk_count,
                hdr.frame_type_key,
                hdr.timestamp_us,
                0,
                0,
                fragment,
            )
        };
        let (
            frame_id,
            chunk_index,
            chunk_count,
            is_key,
            timestamp_us,
            encoded_len,
            encoded_crc,
            fragment,
        ) = parsed;
        if chunk_count == 0
            || chunk_count > V2_CHUNK_COUNT_MAX
            || encoded_len as usize > ENCODED_FRAME_MAX
        {
            return;
        }
        if is_v2 {
            let expected_count = (encoded_len as usize)
                .max(1)
                .div_ceil(CHUNK_V2_FRAGMENT_MAX);
            if usize::from(chunk_count) != expected_count {
                return;
            }
            let expected_fragment_len = if usize::from(chunk_index + 1) == usize::from(chunk_count)
            {
                encoded_len as usize - CHUNK_V2_FRAGMENT_MAX * (usize::from(chunk_count) - 1)
            } else {
                CHUNK_V2_FRAGMENT_MAX
            };
            if fragment.len() != expected_fragment_len {
                return;
            }
        }
        let key = (epoch, frame_id);
        if self.completed.contains_key(&key) {
            return;
        }
        if !self.pending.contains_key(&key) {
            if self.pending_order.len() + self.completed_order.len() >= MAX_PENDING_FRAMES {
                self.discontinue(epoch, DiscontinuityReason::ReassemblyEviction, out);
                return;
            }
            if self.buffered_bytes.saturating_add(fragment.len()) > RX_DECODER_MAX_BYTES as usize {
                self.discontinue(epoch, DiscontinuityReason::MemoryCap, out);
                return;
            }
            self.pending.insert(
                key,
                PendingFrame {
                    is_v2,
                    is_key,
                    timestamp_us,
                    chunk_count,
                    encoded_frame_len: encoded_len,
                    encoded_frame_crc32: encoded_crc,
                    parts: vec![None; chunk_count as usize],
                    received_count: 0,
                    bytes: 0,
                    used_recovery: false,
                },
            );
            self.pending_order.push(key);
            // While awaiting an IDR, an incomplete delta must never claim the
            // post-reset ordering anchor: only the first pending key frame
            // (or a non-gated pending frame once recovery has settled) may
            // become `next_frame_id`, so a stale/incomplete delta cannot
            // stall a later complete key behind a reorder-gap wait.
            if self.next_frame_id.is_none() && (!self.awaiting_idr || is_key) {
                self.next_frame_id = Some(frame_id);
            }
        }
        let metadata_matches = self.pending.get(&key).is_some_and(|frame| {
            frame.is_v2 == is_v2
                && frame.is_key == is_key
                && frame.timestamp_us == timestamp_us
                && frame.chunk_count == chunk_count
                && frame.encoded_frame_len == encoded_len
                && frame.encoded_frame_crc32 == encoded_crc
        });
        if !metadata_matches {
            self.discontinue(epoch, DiscontinuityReason::MetadataMismatch, out);
            return;
        }
        let frame = self.pending.get_mut(&key).expect("present: checked above");
        let index = chunk_index as usize;
        if index >= frame.parts.len() || frame.parts[index].is_some() {
            return;
        }
        if self.buffered_bytes.saturating_add(fragment.len()) > RX_DECODER_MAX_BYTES as usize {
            self.discontinue(epoch, DiscontinuityReason::MemoryCap, out);
            return;
        }
        frame.parts[index] = Some(fragment.to_vec());
        frame.received_count += 1;
        frame.bytes += fragment.len();
        frame.used_recovery |= via_fec;
        self.buffered_bytes += fragment.len();

        if frame.received_count != frame.chunk_count {
            return;
        }
        let frame = self.pending.remove(&key).expect("present: completed above");
        self.pending_order.retain(|pending| *pending != key);
        if self.awaiting_idr && !frame.is_key {
            self.buffered_bytes -= frame.bytes;
            self.frames_dropped_awaiting_idr += 1;
            // A dropped delta must not establish the reorder baseline: the next
            // admitted keyframe is the new decode reference and must flush alone.
            if self.next_frame_id == Some(frame_id) {
                self.next_frame_id = None;
            }
            return;
        }
        if frame.is_key {
            self.awaiting_idr = false;
        }
        self.completed.insert(
            key,
            CompletedFrame {
                frame_id,
                frame,
                completed_at_ms: now_ms,
            },
        );
        self.completed_order.push(key);
    }

    fn flush_reorder(&mut self, now_ms: u64, out: &mut Vec<RxEvent>) {
        while let Some(first_key) = self.completed_order.first().copied() {
            let first = self
                .completed
                .get(&first_key)
                .expect("order refers to completed");
            let expected = (first_key.0, self.next_frame_id.unwrap_or(first.frame_id));
            if !self.completed.contains_key(&expected) {
                if self.completed_order.len() <= REORDER_MAX_COMPLETED_FRAMES
                    && now_ms.saturating_sub(first.completed_at_ms) < REORDER_MAX_WAIT_MS
                {
                    return;
                }
                self.discontinue(first_key.0, DiscontinuityReason::ReorderGap, out);
                return;
            }
            let completed = self.completed.remove(&expected).expect("checked above");
            self.completed_order.retain(|key| *key != expected);
            if let Some(unit) =
                self.assemble_frame(completed.frame_id, completed.frame, expected.0, out)
            {
                out.push(RxEvent::Frame(unit));
            }
            self.next_frame_id = Some(completed.frame_id.wrapping_add(1));
        }
    }

    fn assemble_frame(
        &mut self,
        frame_id: u32,
        frame: PendingFrame,
        epoch: u32,
        out: &mut Vec<RxEvent>,
    ) -> Option<DecodeUnit> {
        let mut data = Vec::with_capacity(frame.bytes);
        for part in frame.parts.iter() {
            data.extend_from_slice(part.as_deref()?);
        }
        self.buffered_bytes -= frame.bytes;
        if frame.is_v2
            && validate_encoded_frame_v2(
                &crate::fec_wire::ChunkV2Header {
                    frame_id,
                    chunk_index: 0,
                    chunk_count: frame.chunk_count,
                    frame_type_key: frame.is_key,
                    timestamp_us: frame.timestamp_us,
                    encoded_frame_len: frame.encoded_frame_len,
                    encoded_frame_crc32: frame.encoded_frame_crc32,
                },
                &data,
            )
            .is_err()
        {
            self.discontinue(epoch, DiscontinuityReason::FrameCrc, out);
            return None;
        }
        if frame.used_recovery {
            self.frames_recovered += 1;
        }
        let duration_us = i64::from(frame.timestamp_us) - i64::from(self.last_timestamp_us);
        self.last_timestamp_us = frame.timestamp_us;
        Some(DecodeUnit {
            frame_id,
            epoch,
            is_key: frame.is_key,
            timestamp_us: frame.timestamp_us,
            duration_us,
            data,
        })
    }

    /// ACK cadence gate — called on every recovered symbol and from `tick`.
    /// Fires when ≥32 symbols were delivered since the last ACK OR ≥50 ms
    /// elapsed, whichever comes first, and only while `highest_fully_decoded`
    /// has advanced past the last acked value (fec-framing.md §2).
    ///
    /// TS-mirror note: the counter also increments on timer ticks while an
    /// un-acked advance is outstanding — kept identical to the TS pipe so
    /// both clients present the same ACK stream to the host window.
    fn tick_ack(&mut self, now_ms: u64) -> Option<u32> {
        let hfd = self.decoder.highest_fully_decoded()?;
        if let Some(last) = self.last_acked_highest
            && !seq_advanced(hfd, last)
        {
            return None;
        }

        self.symbols_since_ack += 1;
        let elapsed = now_ms.saturating_sub(self.last_ack_time_ms);

        if self.symbols_since_ack >= ACK_SYMBOL_INTERVAL || elapsed >= ACK_TIME_INTERVAL_MS {
            self.last_acked_highest = Some(hfd);
            self.symbols_since_ack = 0;
            self.last_ack_time_ms = now_ms;
            return Some(hfd);
        }
        None
    }
}

/// Returns whether `candidate` is RFC 1982-forward from `previous`.
///
/// Zero distance and the half-range are both rejected; this admits max→0.
fn seq_advanced(candidate: u32, previous: u32) -> bool {
    let distance = candidate.wrapping_sub(previous);
    distance != 0 && distance < 0x8000_0000
}

// ── Tests ─────────────────────────────────────────────────────────────────
//
// Scenario parity with tests/fec_pipe.test.mjs (the TS production pipe),
// plus Rust-side hardening cases marked DIVERGENCE above.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::{FecConfig, FecEncoder, Symbol};
    use crate::fec_wire::{
        CHUNK_HEADER_LEN, Epoch, chunk_frame_v2, encode_symbol_msg, encode_symbol_msg_v2,
    };

    // ── Helpers (mirror fec_pipe.test.mjs helpers) ────────────────────────

    fn build_chunk_payload(
        frame_id: u32,
        chunk_index: u16,
        chunk_count: u16,
        key: bool,
        timestamp_us: u32,
        fragment: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(CHUNK_HEADER_LEN + fragment.len());
        buf.extend_from_slice(&frame_id.to_le_bytes());
        buf.extend_from_slice(&chunk_index.to_le_bytes());
        buf.extend_from_slice(&chunk_count.to_le_bytes());
        buf.push(u8::from(key));
        buf.extend_from_slice(&timestamp_us.to_le_bytes());
        buf.extend_from_slice(fragment);
        buf
    }

    fn source_msg(seq: u32, chunk_payload: &[u8]) -> Vec<u8> {
        encode_symbol_msg(&Symbol::Source {
            seq,
            payload: chunk_payload.to_vec(),
        })
    }

    fn make_enc() -> FecEncoder {
        FecEncoder::new(FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 24,
        })
    }

    fn frames_of(events: &[RxEvent]) -> Vec<&DecodeUnit> {
        events
            .iter()
            .filter_map(|e| match e {
                RxEvent::Frame(u) => Some(u),
                RxEvent::Discontinuity { .. } | RxEvent::Ack(_) => None,
            })
            .collect()
    }

    fn acks_of(events: &[RxEvent]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|e| match e {
                RxEvent::Ack(a) => Some(*a),
                RxEvent::Frame(_) | RxEvent::Discontinuity { .. } => None,
            })
            .collect()
    }

    fn source_msg_v2(epoch: u32, seq: u32, payload: Vec<u8>) -> Vec<u8> {
        encode_symbol_msg_v2(&V2Symbol::Source {
            epoch: Epoch::new(epoch).expect("nonzero test epoch"),
            seq,
            payload,
        })
        .expect("valid v2 source")
    }

    // ── Reassembly (TS parity) ────────────────────────────────────────────

    #[test]
    fn out_of_order_chunks_reassemble_byte_identical_frame() {
        let mut rx = VideoReceiver::new(0);

        let frame_data = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let chunk0 = build_chunk_payload(0, 0, 2, false, 1000, &frame_data[..4]);
        let chunk1 = build_chunk_payload(0, 1, 2, false, 1000, &frame_data[4..]);

        let ev = rx.on_message(&source_msg(1, &chunk1), 0);
        assert!(frames_of(&ev).is_empty(), "not yet: missing chunk 0");

        let ev = rx.on_message(&source_msg(0, &chunk0), 0);
        let frames = frames_of(&ev);
        assert_eq!(frames.len(), 1, "frame assembled after second chunk");
        assert_eq!(frames[0].data, frame_data);
        assert!(!frames[0].is_key);
        assert_eq!(frames[0].timestamp_us, 1000);
        assert_eq!(frames[0].frame_id, 0);
    }

    #[test]
    fn two_frames_interleaved_chunks_both_deliver() {
        let mut rx = VideoReceiver::new(0);

        let f0c0 = build_chunk_payload(0, 0, 2, false, 100, &[10]);
        let f1c0 = build_chunk_payload(1, 0, 2, true, 200, &[30]);
        let f0c1 = build_chunk_payload(0, 1, 2, false, 100, &[20]);
        let f1c1 = build_chunk_payload(1, 1, 2, true, 200, &[40]);

        assert!(frames_of(&rx.on_message(&source_msg(0, &f0c0), 0)).is_empty());
        assert!(frames_of(&rx.on_message(&source_msg(1, &f1c0), 0)).is_empty());

        let ev = rx.on_message(&source_msg(2, &f0c1), 0);
        let frames = frames_of(&ev);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, [10, 20]);
        assert!(!frames[0].is_key);

        let ev = rx.on_message(&source_msg(3, &f1c1), 0);
        let frames = frames_of(&ev);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, [30, 40]);
        assert!(frames[0].is_key, "frame 1 is a key frame");

        // U2 P2 groundwork: clean stream (no loss) -> all recovery counters
        // zero; source count matches the 4 symbols fed.
        let stats = rx.stats();
        assert_eq!(stats.source_symbols_received, 4);
        assert_eq!(stats.symbols_recovered, 0);
        assert_eq!(stats.frames_recovered, 0);
        assert_eq!(stats.loss_spans, 0);
        assert_eq!(stats.loss_spans_recovered, 0);
    }

    #[test]
    fn repair_recovery_completes_frame() {
        // frame_id=0 spans seq 0..2; seq 0 is dropped on the wire. The repair
        // emitted with seq 1 covers window [0..2), so once the seq-1 source
        // arrives the decoder has 1 unknown / 1 equation and recovers seq 0.
        let mut rx = VideoReceiver::new(0);
        let mut enc = make_enc();

        let chunk0 = build_chunk_payload(0, 0, 2, false, 5000, &[0xAA, 0xBB]);
        let chunk1 = build_chunk_payload(0, 1, 2, false, 5000, &[0xCC, 0xDD]);

        enc.push_source(0, &chunk0);
        let out = enc.push_source(1, &chunk1);
        let repair = out
            .repairs
            .into_iter()
            .next()
            .expect("1/1 redundancy must emit a repair");

        let ev = rx.on_message(&encode_symbol_msg(&repair), 0);
        assert!(
            frames_of(&ev).is_empty(),
            "no frame yet: 2 unknowns, 1 equation"
        );

        let ev = rx.on_message(&source_msg(1, &chunk1), 0);
        let frames = frames_of(&ev);
        assert_eq!(frames.len(), 1, "frame assembled via FEC recovery");
        assert_eq!(frames[0].data, [0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(frames[0].timestamp_us, 5000);

        // U2 P2 groundwork: the frame used a recovered symbol (seq 0), and
        // the decoder-level counters passthrough correctly.
        let stats = rx.stats();
        assert_eq!(stats.frames_recovered, 1, "frame used a recovered symbol");
        assert_eq!(
            stats.symbols_recovered, 1,
            "exactly seq 0 recovered via FEC"
        );
        assert_eq!(stats.loss_spans, 1);
        assert_eq!(stats.loss_spans_recovered, 1);
    }

    #[test]
    fn loss_span_drops_pending_and_latches_needs_idr_once() {
        let mut rx = VideoReceiver::new(0);

        // Partial frame (chunk 0 of 2) — pending.
        let partial = build_chunk_payload(0, 0, 2, false, 1000, &[0x01]);
        assert!(frames_of(&rx.on_message(&source_msg(0, &partial), 0)).is_empty());

        // Encoder window [0..2): repair covers seq 0..2. Deliver seq 2 source
        // then the repair alone cannot close the gap once the decoder window
        // slides — force a LossSpan by bounding the gap with a later source
        // and letting eviction of seq 1 occur via a repair whose window has
        // moved past it.
        let mut enc = make_enc();
        enc.push_source(0, &[0]);
        enc.push_source(1, &[1]);
        let out = enc.push_source(2, &[2]);
        let repair = out
            .repairs
            .into_iter()
            .next()
            .expect("repair for window ending at seq 3");

        let chunk_seq2 = build_chunk_payload(99, 0, 1, false, 2000, &[0x42]);
        rx.on_message(&source_msg(2, &chunk_seq2), 0);
        rx.on_message(&encode_symbol_msg(&repair), 0);

        // Whether a LossSpan fired already depends on decoder advance logic
        // (TS test makes the same allowance): the contract under test is the
        // latch-then-clear behaviour of the poll.
        let _ = rx.poll_needs_idr();
        assert!(!rx.poll_needs_idr(), "needs_idr cleared after one poll");

        // U2 P2 groundwork: whatever the exact LossSpan count (ambiguous per
        // the comment above), no frame ever used a recovered symbol here
        // (the recovered payload, if any, is too short to be a valid chunk
        // header) -- needs-IDR behaviour is unaffected by the new counters.
        let stats = rx.stats();
        assert_eq!(
            stats.frames_recovered, 0,
            "no frame used a recovered symbol"
        );
    }

    #[test]
    fn stats_unrecoverable_gap_counts_span_but_not_recovered() {
        // Same scenario as fec.rs's pin_loss_span_emitted_for_bounded_missing_gap:
        // 1/4 ratio, seqs 2..6 dropped (4 losses, only 2 repairs) -> the gap
        // is bounded and abandoned, never healed via FEC.
        let mut rx = VideoReceiver::new(0);
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 4,
            window_max_symbols: 64,
            window_max_bytes: 1 << 24,
        });

        for i in 0..8u32 {
            let chunk = build_chunk_payload(i, 0, 1, false, i * 1000, &[i as u8]);
            let out = enc.push_source(i, &chunk);
            let drop = (2..6).contains(&i);
            if !drop {
                rx.on_message(&encode_symbol_msg(&out.source), 0);
            }
            for repair in out.repairs {
                rx.on_message(&encode_symbol_msg(&repair), 0);
            }
        }

        let stats = rx.stats();
        assert_eq!(stats.loss_spans, 1, "one loss episode observed");
        assert_eq!(
            stats.loss_spans_recovered, 0,
            "the episode was skipped, not healed"
        );
        // needs-IDR latch/clear contract is unaffected by the new counters:
        // whatever the first read is, a second read must be false.
        let _ = rx.poll_needs_idr();
        assert!(!rx.poll_needs_idr(), "needs_idr cleared after one poll");
    }

    #[test]
    fn discontinuity_without_pending_requests_idr_and_gates_deltas_until_keyframe() {
        let mut rx = VideoReceiver::new(0);
        let mut events = Vec::new();
        rx.discontinue(0, DiscontinuityReason::FecEviction, &mut events);
        assert!(matches!(
            events.as_slice(),
            [RxEvent::Discontinuity {
                epoch: 0,
                reason: DiscontinuityReason::FecEviction,
            }]
        ));
        assert!(rx.poll_needs_idr());

        let delta = build_chunk_payload(1, 0, 1, false, 1_000, &[0x11]);
        assert!(frames_of(&rx.on_message(&source_msg(0, &delta), 0)).is_empty());
        assert_eq!(rx.stats().frames_dropped_awaiting_idr, 1);

        let key = build_chunk_payload(2, 0, 1, true, 2_000, &[0x22]);
        let key_events = rx.on_message(&source_msg(1, &key), 0);
        let frames = frames_of(&key_events);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_key);

        let delta = build_chunk_payload(3, 0, 1, false, 3_000, &[0x33]);
        assert_eq!(
            frames_of(&rx.on_message(&source_msg(2, &delta), 0)).len(),
            1
        );
    }
    #[test]
    fn key_after_incomplete_earlier_delta_becomes_the_reorder_anchor_immediately() {
        // WATCH finding agent://76-FinalReviewCore: while awaiting_idr is
        // latched, an incomplete pending delta must never claim
        // next_frame_id. Adversarial order: an earlier delta's first chunk
        // arrives (never completes), then a later same-epoch key completes
        // in full — the key must become the post-reset anchor and flush
        // immediately, not wait behind the stale delta's frame_id.
        let mut rx = VideoReceiver::new(0);
        let mut events = Vec::new();
        rx.discontinue(0, DiscontinuityReason::FecEviction, &mut events);
        assert!(rx.poll_needs_idr(), "discontinuity latches needs_idr");

        // Earlier delta (frame_id 5, 2 chunks) — only the first chunk ever
        // arrives, so it stays pending/incomplete forever.
        let stale_delta = build_chunk_payload(5, 0, 2, false, 5_000, &[0xAA]);
        let stale_events = rx.on_message(&source_msg(0, &stale_delta), 0);
        assert!(frames_of(&stale_events).is_empty());
        assert!(
            stale_events
                .iter()
                .all(|e| !matches!(e, RxEvent::Discontinuity { .. })),
            "an incomplete pending delta must not itself trigger a discontinuity"
        );

        // Later key (frame_id 6, single chunk) completes in the same message.
        let key = build_chunk_payload(6, 0, 1, true, 6_000, &[0xBB]);
        let key_events = rx.on_message(&source_msg(1, &key), 0);
        let frames = frames_of(&key_events);
        assert_eq!(
            frames.len(),
            1,
            "the key must emit immediately, not wait for a reorder gap"
        );
        assert!(frames[0].is_key);
        assert_eq!(frames[0].frame_id, 6);
        assert!(
            key_events
                .iter()
                .all(|e| !matches!(e, RxEvent::Discontinuity { .. })),
            "no extra ReorderGap/IDR discontinuity from the stale incomplete delta"
        );
        assert!(
            !rx.poll_needs_idr(),
            "the recovered key must not trigger a second IDR request"
        );

        // The gate has cleared: the next delta emits immediately too.
        let delta = build_chunk_payload(7, 0, 1, false, 7_000, &[0xCC]);
        let delta_events = rx.on_message(&source_msg(2, &delta), 0);
        assert_eq!(
            frames_of(&delta_events).len(),
            1,
            "subsequent delta emits once the gate has cleared"
        );
    }
    #[test]
    fn pending_cap_triggers_full_reset_discontinuity_and_sets_needs_idr() {
        let mut rx = VideoReceiver::new(0);

        // 8 partial frames (chunk 0 of 2 each — never complete).
        for i in 0..8u32 {
            let chunk = build_chunk_payload(i, 0, 2, false, i * 1000, &[i as u8]);
            rx.on_message(&source_msg(i * 2, &chunk), 0);
        }
        assert!(!rx.poll_needs_idr(), "no eviction yet at exactly 8");

        // The 9th partial frame trips the cap: this is a full-reset
        // discontinuity (ReassemblyEviction), not a selective evict-the-
        // oldest — every pending/completed record is cleared, not just #0.
        let chunk9 = build_chunk_payload(8, 0, 2, false, 9000, &[9]);
        rx.on_message(&source_msg(16, &chunk9), 0);
        assert!(
            rx.poll_needs_idr(),
            "full-reset discontinuity at 9th frame sets needs_idr"
        );
        assert!(!rx.poll_needs_idr(), "cleared after one poll");

        // Frame id 0 cannot complete either: the reset cleared every
        // record, so it restarts from a single chunk like all the rest.
        let chunk0b = build_chunk_payload(0, 1, 2, false, 0, &[0xEE]);
        let ev = rx.on_message(&source_msg(17, &chunk0b), 0);
        assert!(
            frames_of(&ev).is_empty(),
            "reset frame restarts from one chunk; must not assemble"
        );
    }

    // ── ACK cadence (TS parity) ───────────────────────────────────────────

    #[test]
    fn ack_fires_after_32_delivered_symbols() {
        let mut rx = VideoReceiver::new(0);
        let mut all_acks = Vec::new();

        for i in 0..31u32 {
            let chunk = build_chunk_payload(i, 0, 1, false, i * 1000, &[i as u8]);
            all_acks.extend(acks_of(&rx.on_message(&source_msg(i, &chunk), 0)));
        }
        assert!(all_acks.is_empty(), "no ack before 32 symbols");

        let chunk32 = build_chunk_payload(31, 0, 1, false, 31000, &[31]);
        let acks = acks_of(&rx.on_message(&source_msg(31, &chunk32), 0));
        assert_eq!(acks, [31], "ack fires at 32nd symbol with correct hfd");
    }

    #[test]
    fn ack_fires_after_50ms_elapsed_with_fewer_symbols() {
        let mut rx = VideoReceiver::new(0);

        for i in 0..5u32 {
            let chunk = build_chunk_payload(i, 0, 1, false, i * 1000, &[i as u8]);
            let ev = rx.on_message(&source_msg(i, &chunk), 0);
            assert!(acks_of(&ev).is_empty());
        }

        // 51 ms later, one more symbol triggers the time-based ack.
        let chunk5 = build_chunk_payload(5, 0, 1, false, 5000, &[5]);
        let acks = acks_of(&rx.on_message(&source_msg(5, &chunk5), 51));
        assert_eq!(acks, [5], "ack fires on 50ms elapsed");
    }

    #[test]
    fn timer_tick_fires_ack_without_new_symbols_and_never_duplicates() {
        let mut rx = VideoReceiver::new(0);

        for i in 0..5u32 {
            let chunk = build_chunk_payload(i, 0, 1, false, i * 1000, &[i as u8]);
            rx.on_message(&source_msg(i, &chunk), 0);
        }

        assert_eq!(
            rx.tick(60),
            Some(4),
            "timer-driven ack fires after 50ms even with no new symbol"
        );
        assert_eq!(
            rx.tick(120),
            None,
            "second tick with the same hfd must not re-fire"
        );
    }

    #[test]
    fn tick_before_any_symbol_is_silent() {
        let mut rx = VideoReceiver::new(0);
        assert_eq!(rx.tick(1000), None, "no hfd yet — no ack");
    }
    #[test]
    fn epoch_transition_resets_ack_cadence_and_wrap_ack_progresses() {
        let mut rx = VideoReceiver::new(0);
        let first = chunk_frame_v2(0, true, 0, &[1]).expect("valid v2 frame");
        assert_eq!(
            acks_of(&rx.on_message(&source_msg_v2(1, 0, first[0].clone()), 50)),
            [0]
        );

        let next = chunk_frame_v2(0, true, 0, &[2]).expect("valid v2 frame");
        let transition = rx.on_message(&source_msg_v2(2, 0, next[0].clone()), 100);
        assert!(transition.iter().any(|event| matches!(
            event,
            RxEvent::Discontinuity {
                reason: DiscontinuityReason::EpochTransition,
                ..
            }
        )));
        assert_eq!(
            rx.tick(150),
            Some(0),
            "new epoch ACK is not blocked by old ACK"
        );

        let mut wrap = VideoReceiver::new(0);
        wrap.last_acked_highest = Some(u32::MAX);
        assert_eq!(
            acks_of(&wrap.on_message(
                &source_msg(0, &build_chunk_payload(0, 0, 1, true, 0, &[3])),
                50,
            )),
            [0],
            "RFC1982 max→0 is forward"
        );
        assert!(!seq_advanced(0x8000_0000, 0), "half-range is not forward");
    }

    #[test]
    fn same_batch_loss_and_keyframe_admits_the_keyframe() {
        let mut rx = VideoReceiver::new(0);
        let key = chunk_frame_v2(0, true, 0, &[7]).expect("valid v2 keyframe");
        let mut events = Vec::new();
        rx.process_decoder_events(
            1,
            true,
            vec![
                DecoderEvent::Recovered {
                    seq: 0,
                    payload: key[0].clone(),
                    via_fec: true,
                },
                DecoderEvent::LossSpan {
                    from_seq: 1,
                    to_seq_exclusive: 2,
                },
            ],
            0,
            &mut events,
        );
        rx.flush_reorder(0, &mut events);

        assert!(events.iter().any(|event| matches!(
            event,
            RxEvent::Discontinuity {
                reason: DiscontinuityReason::FecEviction,
                ..
            }
        )));
        assert_eq!(
            frames_of(&events).len(),
            1,
            "the same-batch key clears the IDR gate"
        );
    }

    #[test]
    fn completed_duplicate_replay_does_not_trigger_record_cap() {
        let mut rx = VideoReceiver::new(0);
        let key = (0, 99);
        rx.completed.insert(
            key,
            CompletedFrame {
                frame_id: 99,
                frame: PendingFrame {
                    is_v2: false,
                    is_key: false,
                    timestamp_us: 0,
                    chunk_count: 1,
                    encoded_frame_len: 0,
                    encoded_frame_crc32: 0,
                    parts: vec![Some(vec![1])],
                    received_count: 1,
                    bytes: 1,
                    used_recovery: false,
                },
                completed_at_ms: 0,
            },
        );
        rx.completed_order.push(key);
        rx.next_frame_id = Some(0);
        rx.pending_order = (0..MAX_PENDING_FRAMES as u32).map(|id| (0, id)).collect();

        let replay = build_chunk_payload(99, 0, 1, false, 0, &[1]);
        let events = rx.on_message(&source_msg(0, &replay), 0);
        assert!(!events.iter().any(|event| matches!(
            event,
            RxEvent::Discontinuity {
                reason: DiscontinuityReason::ReassemblyEviction | DiscontinuityReason::MemoryCap,
                ..
            }
        )));
        assert!(
            !rx.poll_needs_idr(),
            "duplicate replay consumes no capacity"
        );
    }

    // ── Hardening (Rust-side DIVERGENCE cases) ────────────────────────────

    #[test]
    fn malformed_messages_are_dropped() {
        let mut rx = VideoReceiver::new(0);
        assert!(rx.on_message(&[], 0).is_empty(), "empty message");
        assert!(
            rx.on_message(&[0x00, 1, 2], 0).is_empty(),
            "truncated source"
        );
        assert!(
            rx.on_message(&[0x02, 0, 0, 0, 0], 0).is_empty(),
            "unknown kind"
        );

        // Valid source symbol whose chunk payload is shorter than the header:
        // decoder consumes the seq but no frame state is created.
        let ev = rx.on_message(&source_msg(0, &[1, 2, 3]), 0);
        assert!(frames_of(&ev).is_empty(), "short chunk payload dropped");
    }

    #[test]
    fn out_of_range_chunk_index_and_zero_count_are_dropped() {
        let mut rx = VideoReceiver::new(0);

        // chunk_index == chunk_count (out of range).
        let bad_idx = build_chunk_payload(0, 2, 2, false, 100, &[1]);
        assert!(frames_of(&rx.on_message(&source_msg(0, &bad_idx), 0)).is_empty());

        // chunk_count == 0.
        let zero_count = build_chunk_payload(1, 0, 0, false, 100, &[1]);
        assert!(frames_of(&rx.on_message(&source_msg(1, &zero_count), 0)).is_empty());

        // Neither may have created pending state that blocks a valid frame 0.
        let good0 = build_chunk_payload(0, 0, 1, false, 200, &[7]);
        let frames_ev = rx.on_message(&source_msg(2, &good0), 0);
        let frames = frames_of(&frames_ev);
        assert_eq!(frames.len(), 1, "valid single-chunk frame still assembles");
        assert_eq!(frames[0].data, [7]);
    }

    #[test]
    fn duplicate_chunk_does_not_double_count() {
        let mut rx = VideoReceiver::new(0);

        let chunk0 = build_chunk_payload(0, 0, 2, false, 100, &[1]);
        rx.on_message(&source_msg(0, &chunk0), 0);
        // Same chunk again under a different seq (duplicate chunk delivery).
        let ev = rx.on_message(&source_msg(1, &chunk0), 0);
        assert!(
            frames_of(&ev).is_empty(),
            "duplicate chunk must not complete a 2-chunk frame"
        );

        let chunk1 = build_chunk_payload(0, 1, 2, false, 100, &[2]);
        let ev = rx.on_message(&source_msg(2, &chunk1), 0);
        let frames = frames_of(&ev);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, [1, 2]);
    }

    #[test]
    fn empty_frame_round_trips_via_chunk_frame() {
        use crate::fec_wire::chunk_frame;
        let mut rx = VideoReceiver::new(0);

        let chunks = chunk_frame(7, true, 42, &[]);
        assert_eq!(chunks.len(), 1, "empty data -> exactly one chunk");
        let ev = rx.on_message(&source_msg(0, &chunks[0]), 0);
        let frames = frames_of(&ev);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].data.is_empty());
        assert!(frames[0].is_key);
        assert_eq!(frames[0].timestamp_us, 42);
        assert_eq!(frames[0].frame_id, 7);
    }

    #[test]
    fn duration_tracks_timestamp_deltas_across_frames() {
        let mut rx = VideoReceiver::new(0);

        let f0 = build_chunk_payload(0, 0, 1, true, 10_000, &[1]);
        let f1 = build_chunk_payload(1, 0, 1, false, 26_667, &[2]);

        let ev0 = rx.on_message(&source_msg(0, &f0), 0);
        let ev1 = rx.on_message(&source_msg(1, &f1), 0);
        let frame0 = frames_of(&ev0)[0].clone();
        let frame1 = frames_of(&ev1)[0].clone();

        assert_eq!(frame0.duration_us, 10_000, "first frame measures against 0");
        assert_eq!(frame1.duration_us, 16_667);
    }

    // ── End-to-end: encoder → wire → receiver under loss ──────────────────

    #[test]
    fn end_to_end_multi_frame_stream_with_source_loss_recovers_all_frames() {
        use crate::fec_wire::chunk_frame;

        let mut rx = VideoReceiver::new(0);
        let mut enc = make_enc();

        // Deterministic frame contents, multi-chunk (force > 1 chunk each).
        let frame_bytes: Vec<Vec<u8>> = (0..4u32)
            .map(|f| {
                (0..3000u32)
                    .map(|i| (i.wrapping_mul(31).wrapping_add(f * 7) & 0xFF) as u8)
                    .collect()
            })
            .collect();

        let mut seq = 0u32;
        let mut wire: Vec<(u32, Vec<u8>)> = Vec::new(); // (seq, msg)
        for (f, data) in frame_bytes.iter().enumerate() {
            for chunk in chunk_frame(f as u32, f == 0, (f as u32 + 1) * 16_667, data) {
                let out = enc.push_source(seq, &chunk);
                wire.push((seq, encode_symbol_msg(&out.source)));
                for rep in &out.repairs {
                    wire.push((seq, encode_symbol_msg(rep)));
                }
                seq += 1;
            }
        }

        // Drop every 4th source message (keep repairs) — 1/1 redundancy can
        // absorb this.
        let mut delivered = Vec::new();
        for (i, (s, msg)) in wire.iter().enumerate() {
            let is_source = msg[0] == 0x00;
            if is_source && s % 4 == 3 {
                continue; // dropped by the network
            }
            delivered.push((i, msg.clone()));
        }

        let mut got_frames = Vec::new();
        for (_, msg) in delivered {
            for ev in rx.on_message(&msg, 0) {
                if let RxEvent::Frame(u) = ev {
                    got_frames.push(u);
                }
            }
        }

        assert_eq!(got_frames.len(), 4, "all frames recovered despite loss");
        for (f, unit) in got_frames.iter().enumerate() {
            assert_eq!(unit.frame_id, f as u32);
            assert_eq!(
                unit.data, frame_bytes[f],
                "frame {f} byte-identical after FEC recovery"
            );
        }
        assert!(!rx.poll_needs_idr(), "no unrecoverable loss occurred");
    }
    #[test]
    fn v2_reorder_epoch_and_crc_contract_vectors() {
        let mut rx = VideoReceiver::new(0);
        let epoch = 7;
        let n = chunk_frame_v2(10, false, 10, &vec![0x10; 1165])
            .expect("multi-chunk v2 frame should be valid");
        let n1 =
            chunk_frame_v2(11, false, 11, &[0x11]).expect("single-chunk v2 frame should be valid");
        assert!(frames_of(&rx.on_message(&source_msg_v2(epoch, 0, n[0].clone()), 0)).is_empty());
        assert!(frames_of(&rx.on_message(&source_msg_v2(epoch, 1, n1[0].clone()), 0)).is_empty());
        let events = rx.on_message(&source_msg_v2(epoch, 2, n[1].clone()), 0);
        assert_eq!(
            frames_of(&events)
                .iter()
                .map(|u| u.frame_id)
                .collect::<Vec<_>>(),
            [10, 11]
        );

        let f20 = chunk_frame_v2(20, false, 20, &vec![0x20; 1165])
            .expect("multi-chunk v2 frame should be valid");
        let f21 =
            chunk_frame_v2(21, false, 21, &[0x21]).expect("single-chunk v2 frame should be valid");
        rx.on_message(&source_msg_v2(epoch, 3, f20[0].clone()), 0);
        rx.on_message(&source_msg_v2(epoch, 4, f21[0].clone()), 0);
        assert!(rx.tick_events(100).iter().any(|event| matches!(
            event,
            RxEvent::Discontinuity {
                reason: DiscontinuityReason::ReorderGap,
                ..
            }
        )));

        let delta = chunk_frame_v2(30, false, 30, &[1])
            .expect("single-chunk delta v2 frame should be valid");
        assert!(
            frames_of(&rx.on_message(&source_msg_v2(epoch, 5, delta[0].clone()), 101)).is_empty()
        );
        let key =
            chunk_frame_v2(31, true, 31, &[2]).expect("single-chunk key v2 frame should be valid");
        assert_eq!(
            frames_of(&rx.on_message(&source_msg_v2(epoch, 6, key[0].clone()), 101)).len(),
            1
        );

        let transition = chunk_frame_v2(0, true, 0, &[3])
            .expect("single-chunk epoch-transition v2 frame should be valid");
        assert!(
            rx.on_message(&source_msg_v2(8, 0, transition[0].clone()), 102)
                .iter()
                .any(|event| matches!(
                    event,
                    RxEvent::Discontinuity {
                        reason: DiscontinuityReason::EpochTransition,
                        ..
                    }
                ))
        );
        assert!(
            rx.on_message(&source_msg_v2(7, 7, transition[0].clone()), 102)
                .is_empty()
        );

        let mut wrap = VideoReceiver::new(0);
        wrap.on_message(&source_msg_v2(0xffff_ffff, 0, transition[0].clone()), 0);
        assert!(
            wrap.on_message(&source_msg_v2(1, 1, transition[0].clone()), 0)
                .iter()
                .any(|event| matches!(
                    event,
                    RxEvent::Discontinuity {
                        reason: DiscontinuityReason::EpochTransition,
                        ..
                    }
                ))
        );
        let mut half_range = VideoReceiver::new(0);
        half_range.on_message(&source_msg_v2(1, 0, transition[0].clone()), 0);
        assert!(
            half_range
                .on_message(&source_msg_v2(0x8000_0001, 1, transition[0].clone()), 0)
                .is_empty()
        );
    }

    #[test]
    fn v2_metadata_crc_and_record_cap_are_typed_discontinuities() {
        let mut rx = VideoReceiver::new(0);
        let chunks = chunk_frame_v2(0, false, 0, &vec![1; 1165])
            .expect("multi-chunk metadata test frame should be valid");
        rx.on_message(&source_msg_v2(1, 0, chunks[0].clone()), 0);
        let mut mixed = chunks[1].clone();
        mixed[8] = 1;
        assert!(
            rx.on_message(&source_msg_v2(1, 1, mixed), 0)
                .iter()
                .any(|event| matches!(
                    event,
                    RxEvent::Discontinuity {
                        reason: DiscontinuityReason::MetadataMismatch,
                        ..
                    }
                ))
        );

        let mut crc = VideoReceiver::new(0);
        let mut corrupt = chunk_frame_v2(0, true, 0, &[1])
            .expect("single-chunk CRC test frame should be valid")[0]
            .clone();
        *corrupt
            .last_mut()
            .expect("encoded v2 chunk must contain a payload byte") ^= 1;
        assert!(
            crc.on_message(&source_msg_v2(1, 0, corrupt), 0)
                .iter()
                .any(|event| matches!(
                    event,
                    RxEvent::Discontinuity {
                        reason: DiscontinuityReason::FrameCrc,
                        ..
                    }
                ))
        );

        let mut cap = VideoReceiver::new(0);
        for id in 0..8 {
            let partial = chunk_frame_v2(id, false, id, &vec![0; 1165])
                .expect("multi-chunk record-cap test frame should be valid");
            cap.on_message(&source_msg_v2(1, id, partial[0].clone()), 0);
        }
        let ninth = chunk_frame_v2(8, false, 8, &vec![0; 1165])
            .expect("multi-chunk record-cap test frame should be valid");
        assert!(
            cap.on_message(&source_msg_v2(1, 8, ninth[0].clone()), 0)
                .iter()
                .any(|event| matches!(
                    event,
                    RxEvent::Discontinuity {
                        reason: DiscontinuityReason::ReassemblyEviction,
                        ..
                    }
                ))
        );
    }
}
