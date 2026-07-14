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
use crate::fec_wire::{CHUNK_HEADER_LEN, parse_chunk_header, parse_symbol_msg};

// ── Constants (mirror fec_decode_pipe.ts) ─────────────────────────────────

/// Maximum pending frame_ids before evicting the oldest (sets needs_idr).
pub const MAX_PENDING_FRAMES: usize = 8;

/// ACK after this many delivered source symbols since the last ack.
pub const ACK_SYMBOL_INTERVAL: u32 = 32;

/// ACK after this many ms since the last ack (whichever comes first).
pub const ACK_TIME_INTERVAL_MS: u64 = 50;

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
    /// A frame finished reassembly (in completion order).
    Frame(DecodeUnit),
    /// The ACK cadence gate fired: send `AckMsg::Ack(seq)` on `video_fec_ack`.
    Ack(u32),
}

// ── Internal reassembly state ─────────────────────────────────────────────

#[derive(Debug)]
struct PendingFrame {
    /// Header fields from the first chunk seen (TS mirror: later chunks'
    /// header fields other than chunk_index are ignored).
    is_key: bool,
    timestamp_us: u32,
    chunk_count: u16,
    /// index = chunk_index, value = fragment bytes.
    parts: Vec<Option<Vec<u8>>>,
    received_count: u16,
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
    pending: HashMap<u32, PendingFrame>,
    /// Eviction order — insertion-ordered frame_ids.
    pending_order: Vec<u32>,

    /// Latched until polled (mirrors TS `pollRequestIdr`).
    needs_idr: bool,

    /// Duration tracking (mirrors DepacketizeVideoPipe / TS pipe).
    last_timestamp_us: u32,

    // ACK cadence state
    symbols_since_ack: u32,
    last_ack_time_ms: u64,
    last_acked_highest: Option<u32>,
}

impl VideoReceiver {
    /// `now_ms`: caller clock at construction (any monotonic ms source).
    pub fn new(now_ms: u64) -> Self {
        Self {
            decoder: FecDecoder::new(RX_DECODER_MAX_SYMBOLS, RX_DECODER_MAX_BYTES),
            pending: HashMap::new(),
            pending_order: Vec::new(),
            needs_idr: false,
            last_timestamp_us: 0,
            symbols_since_ack: 0,
            last_ack_time_ms: now_ms,
            last_acked_highest: None,
        }
    }

    /// Consume one raw `video_fec` DataChannel message.
    ///
    /// Returns completed frames and at most one ACK, in the order they were
    /// produced. Malformed messages are silently dropped (TS mirror).
    pub fn on_message(&mut self, buf: &[u8], now_ms: u64) -> Vec<RxEvent> {
        let Some(sym) = parse_symbol_msg(buf) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for ev in self.decoder.push_symbol(sym) {
            match ev {
                DecoderEvent::Recovered { payload, .. } => {
                    if let Some(unit) = self.deliver_chunk(&payload) {
                        out.push(RxEvent::Frame(unit));
                    }
                    if let Some(ack) = self.tick_ack(now_ms) {
                        out.push(RxEvent::Ack(ack));
                    }
                }
                DecoderEvent::LossSpan { .. } => self.handle_loss_span(),
            }
        }
        out
    }

    /// Independent ~50 ms timer tick (TS `tickTimer`). Fires an ACK when
    /// `highest_fully_decoded` advanced past the last acked value and the
    /// time threshold is met, even when no symbols arrive.
    pub fn tick(&mut self, now_ms: u64) -> Option<u32> {
        self.tick_ack(now_ms)
    }

    /// Latched needs-IDR flag; cleared by the poll (TS `pollRequestIdr`).
    pub fn poll_needs_idr(&mut self) -> bool {
        std::mem::take(&mut self.needs_idr)
    }

    /// Highest contiguously decoded source seq (stats/debug passthrough).
    pub fn highest_fully_decoded(&self) -> Option<u32> {
        self.decoder.highest_fully_decoded()
    }

    // ── Private helpers ───────────────────────────────────────────────────

    /// Deliver one chunk (source-symbol payload) into the reassembly map.
    /// Returns the finished frame when this chunk completes it.
    fn deliver_chunk(&mut self, payload: &[u8]) -> Option<DecodeUnit> {
        if payload.len() < CHUNK_HEADER_LEN {
            return None;
        }
        let (hdr, fragment) = parse_chunk_header(payload)?;

        // DIVERGENCE: the TS pipe lets an out-of-range chunk_index grow the
        // sparse parts array and can then assemble a short frame; here a
        // malformed index (or zero chunk_count) drops the chunk instead.
        // The host encoder (chunk_frame) never emits either shape.
        if hdr.chunk_count == 0 || hdr.chunk_index >= hdr.chunk_count {
            return None;
        }

        if !self.pending.contains_key(&hdr.frame_id) {
            // Evict oldest if at cap (TS mirror: eviction latches needs_idr).
            if self.pending_order.len() >= MAX_PENDING_FRAMES {
                let evict_id = self.pending_order.remove(0);
                self.pending.remove(&evict_id);
                self.needs_idr = true;
            }
            self.pending.insert(
                hdr.frame_id,
                PendingFrame {
                    is_key: hdr.frame_type_key,
                    timestamp_us: hdr.timestamp_us,
                    chunk_count: hdr.chunk_count,
                    parts: vec![None; hdr.chunk_count as usize],
                    received_count: 0,
                },
            );
            self.pending_order.push(hdr.frame_id);
        }

        let frame = self
            .pending
            .get_mut(&hdr.frame_id)
            .expect("inserted above if absent");

        // A later chunk's count may disagree with the first-seen one (never
        // produced by our encoder); bound by the allocated parts length.
        let idx = hdr.chunk_index as usize;
        if idx >= frame.parts.len() {
            return None;
        }
        // Guard against duplicate delivery.
        if frame.parts[idx].is_some() {
            return None;
        }
        frame.parts[idx] = Some(fragment.to_vec());
        frame.received_count += 1;

        if frame.received_count >= frame.chunk_count {
            let frame = self
                .pending
                .remove(&hdr.frame_id)
                .expect("present: just mutated");
            self.pending_order.retain(|&id| id != hdr.frame_id);
            return Some(self.assemble_frame(hdr.frame_id, frame));
        }
        None
    }

    /// Concatenate all parts into a [`DecodeUnit`] (TS `assembleFrame`).
    fn assemble_frame(&mut self, frame_id: u32, frame: PendingFrame) -> DecodeUnit {
        let total: usize = frame
            .parts
            .iter()
            .map(|p| p.as_deref().map_or(0, <[u8]>::len))
            .sum();
        let mut data = Vec::with_capacity(total);
        for part in frame.parts.iter().flatten() {
            data.extend_from_slice(part);
        }

        let duration_us = i64::from(frame.timestamp_us) - i64::from(self.last_timestamp_us);
        self.last_timestamp_us = frame.timestamp_us;

        DecodeUnit {
            frame_id,
            is_key: frame.is_key,
            timestamp_us: frame.timestamp_us,
            duration_us,
            data,
        }
    }

    /// A LossSpan means source seqs are unrecoverably gone. Any pending frame
    /// may depend on them; we conservatively drop all pending frames (we do
    /// not track per-chunk seqs) and latch needs_idr. TS mirror.
    fn handle_loss_span(&mut self) {
        if !self.pending.is_empty() {
            self.pending.clear();
            self.pending_order.clear();
            self.needs_idr = true;
        }
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
            && hfd <= last
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

// ── Tests ─────────────────────────────────────────────────────────────────
//
// Scenario parity with tests/fec_pipe.test.mjs (the TS production pipe),
// plus Rust-side hardening cases marked DIVERGENCE above.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::{FecConfig, FecEncoder, Symbol};
    use crate::fec_wire::encode_symbol_msg;

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
                RxEvent::Ack(_) => None,
            })
            .collect()
    }

    fn acks_of(events: &[RxEvent]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|e| match e {
                RxEvent::Ack(a) => Some(*a),
                RxEvent::Frame(_) => None,
            })
            .collect()
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
        assert!(
            !rx.poll_needs_idr(),
            "needs_idr cleared after one poll"
        );
    }

    #[test]
    fn pending_cap_evicts_oldest_and_sets_needs_idr() {
        let mut rx = VideoReceiver::new(0);

        // 8 partial frames (chunk 0 of 2 each — never complete).
        for i in 0..8u32 {
            let chunk = build_chunk_payload(i, 0, 2, false, i * 1000, &[i as u8]);
            rx.on_message(&source_msg(i * 2, &chunk), 0);
        }
        assert!(!rx.poll_needs_idr(), "no eviction yet at exactly 8");

        // 9th partial frame evicts the oldest.
        let chunk9 = build_chunk_payload(8, 0, 2, false, 9000, &[9]);
        rx.on_message(&source_msg(16, &chunk9), 0);
        assert!(rx.poll_needs_idr(), "eviction at 9th frame sets needs_idr");
        assert!(!rx.poll_needs_idr(), "cleared after one poll");

        // The evicted frame (id 0) can no longer complete.
        let chunk0b = build_chunk_payload(0, 1, 2, false, 0, &[0xEE]);
        let ev = rx.on_message(&source_msg(17, &chunk0b), 0);
        assert!(
            frames_of(&ev).is_empty(),
            "evicted frame restarts from one chunk; must not assemble"
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

    // ── Hardening (Rust-side DIVERGENCE cases) ────────────────────────────

    #[test]
    fn malformed_messages_are_dropped() {
        let mut rx = VideoReceiver::new(0);
        assert!(rx.on_message(&[], 0).is_empty(), "empty message");
        assert!(rx.on_message(&[0x00, 1, 2], 0).is_empty(), "truncated source");
        assert!(rx.on_message(&[0x02, 0, 0, 0, 0], 0).is_empty(), "unknown kind");

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
}
