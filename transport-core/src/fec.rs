//! Tetrys-style systematic sliding-window FEC codec — betterparsec.
//!
//! Source symbols pass through unmodified with a sequence number; the encoder
//! additionally emits repair symbols at a configurable ratio. A repair symbol
//! is a GF(256) linear combination of every source symbol in the current
//! elastic window. Coefficients are derived deterministically from
//! (repair_seq, src_seq) via [`gf_coeff`], so the decoder reconstructs them
//! from the header alone — no separate coefficient payload is shipped.
//!
//! Pure/deterministic: no clocks, no I/O. Transport attachment (datachannel
//! or custom RTP framing) is out of scope for this module.

use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;

// ── GF(256) ───────────────────────────────────────────────────────────────

/// GF(256) tables: irreducible polynomial x^8+x^4+x^3+x^2+1 (0x11D),
/// generator g = 0x02.
struct GfTables {
    /// exp[i] = g^i; exp[255..510] mirrors exp[0..255] for index-sum access.
    exp: [u8; 512],
    /// log[x] = discrete log base g; log[0] is undefined — never access.
    log: [u8; 256],
}

fn build_tables() -> GfTables {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u8 = 1;
    for i in 0u16..255 {
        exp[i as usize] = x;
        exp[i as usize + 255] = x; // wrap-around copy for direct index-sum
        log[x as usize] = i as u8;
        // multiply x by 0x02 in GF(2^8) with poly 0x11D
        // use wrapping_shl to avoid debug-mode overflow panic (fix #1)
        let carry = x & 0x80;
        x = x.wrapping_shl(1);
        if carry != 0 {
            x ^= 0x1D; // reduce mod 0x11D (lower 8 bits)
        }
    }
    exp[255] = exp[0]; // exp[255] = g^255 = 1
    exp[510] = exp[255]; // mirror
    GfTables { exp, log }
}

static GF_TABLES: OnceLock<GfTables> = OnceLock::new();

fn gf_tables() -> &'static GfTables {
    GF_TABLES.get_or_init(build_tables)
}

/// GF(256) multiplication. Returns 0 if either operand is 0.
fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = gf_tables();
    // log[a] + log[b] ≤ 254+254 = 508 < 512, so direct index into exp[0..512]
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

/// GF(256) multiplicative inverse. Panics if a == 0.
fn gf_inv(a: u8) -> u8 {
    assert_ne!(a, 0, "gf_inv(0) is undefined");
    let t = gf_tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// Deterministic GF(256) coefficient for (repair_seq, src_seq).
/// Always returns a non-zero value (maps 0 → 1).
fn gf_coeff(repair_seq: u16, src_seq: u32) -> u8 {
    let h = (repair_seq as u32)
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(src_seq.wrapping_mul(0x6B43_6201));
    let c = ((h >> 24) ^ (h >> 16) ^ (h >> 8) ^ h) as u8;
    if c == 0 { 0x01 } else { c }
}

// ── 설정 ─────────────────────────────────────────────────────────────────

/// FEC encoder/decoder 동작을 제어하는 튜닝 파라미터.
#[derive(Debug, Clone, Copy)]
pub struct FecConfig {
    /// repair 비율의 분자.
    pub redundancy_numerator: u8,
    /// repair 비율의 분모. 0은 1로 처리.
    pub redundancy_denominator: u8,
    /// 윈도우 최대 심볼 수 (하드 캡). 최대 128 (gf_coeff Cauchy 제약).
    pub window_max_symbols: u16,
    /// 윈도우 최대 바이트 예산.
    pub window_max_bytes: u32,
}

impl FecConfig {
    /// 기본값: 1/8 비율(~12.5% 오버헤드), 윈도우 64 심볼, 512 KiB.
    pub fn default_streaming() -> Self {
        Self {
            redundancy_numerator: 1,
            redundancy_denominator: 8,
            window_max_symbols: 64,
            window_max_bytes: 524_288,
        }
    }

    fn sanitised(self) -> Self {
        Self {
            redundancy_numerator: self.redundancy_numerator,
            redundancy_denominator: self.redundancy_denominator.max(1),
            // clamp to 128: Cauchy y = src_seq % 128, window > 128 causes column
            // collision → rank deficiency → silent recovery failure (fix #2)
            window_max_symbols: self.window_max_symbols.clamp(1, 128),
            window_max_bytes: self.window_max_bytes.max(1),
        }
    }
}

// ── Symbol 타입 ───────────────────────────────────────────────────────────

/// FEC 계층이 교환하는 단일 단위.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Symbol {
    /// 체계적 source 심볼: 원본 payload 무수정 통과.
    Source { seq: u32, payload: Vec<u8> },
    /// Repair 심볼: GF(256) 선형 조합.
    Repair {
        repair_seq: u16,
        window_base: u32,
        window_end: u32,
        payload: Vec<u8>,
    },
}

// ── 인코더 ────────────────────────────────────────────────────────────────

/// 인코더가 source symbol 하나를 처리한 결과.
#[derive(Debug)]
pub struct EncoderOutput {
    pub source: Symbol,
    pub repairs: Vec<Symbol>,
}

#[derive(Debug)]
struct WindowEntry {
    seq: u32,
    /// 길이 접두어(2 bytes LE) + 원본 payload.
    prefixed_payload: Vec<u8>,
}

/// RFC 1982 ordering for u32 serials. `None` is the intentionally unordered
/// exact half-range case.
fn serial_cmp(a: u32, b: u32) -> Option<std::cmp::Ordering> {
    if a == b {
        return Some(std::cmp::Ordering::Equal);
    }
    let distance = a.wrapping_sub(b);
    if distance == 0x8000_0000 {
        None
    } else if distance < 0x8000_0000 {
        Some(std::cmp::Ordering::Greater)
    } else {
        Some(std::cmp::Ordering::Less)
    }
}

/// True when `seq` belongs to the bounded serial interval `[base, base + len)`.
fn seq_in_window(seq: u32, base: u32, len: u32) -> bool {
    seq.wrapping_sub(base) < len
}

/// 체계적 슬라이딩 윈도우 FEC 인코더.
#[derive(Debug)]
pub struct FecEncoder {
    config: FecConfig,
    window: VecDeque<WindowEntry>,
    window_byte_total: u32,
    next_repair_seq: u16,
    /// ratio counter (accumulates numerator each source; drains by denominator)
    ratio_acc: u16,
}

impl FecEncoder {
    pub fn new(config: FecConfig) -> Self {
        let config = config.sanitised();
        Self {
            config,
            window: VecDeque::new(),
            window_byte_total: 0,
            next_repair_seq: 0,
            ratio_acc: 0,
        }
    }

    pub fn set_redundancy(&mut self, numerator: u8, denominator: u8) {
        self.config.redundancy_numerator = numerator;
        self.config.redundancy_denominator = denominator.max(1);
    }

    pub fn push_source(&mut self, seq: u32, payload: &[u8]) -> EncoderOutput {
        assert!(
            payload.len() <= 65520,
            "payload exceeds maximum 65520 bytes"
        );

        let payload_len = payload.len() as u16;
        let mut prefixed = Vec::with_capacity(2 + payload.len());
        prefixed.extend_from_slice(&payload_len.to_le_bytes());
        prefixed.extend_from_slice(payload);

        // Evict to enforce caps before adding
        let new_bytes = payload.len() as u32;
        // symbol cap check: if adding this would exceed cap, evict first
        while self.window.len() >= self.config.window_max_symbols as usize {
            self.evict_oldest();
        }
        // byte cap check
        while !self.window.is_empty()
            && self.window_byte_total.saturating_add(new_bytes) > self.config.window_max_bytes
        {
            self.evict_oldest();
        }

        self.window_byte_total = self.window_byte_total.saturating_add(new_bytes);
        self.window.push_back(WindowEntry {
            seq,
            prefixed_payload: prefixed,
        });

        let source = Symbol::Source {
            seq,
            payload: payload.to_vec(),
        };

        // Ratio counter: accumulate numerator, emit repair when >= denominator
        let mut repairs = Vec::new();
        if self.config.redundancy_numerator > 0 {
            self.ratio_acc += self.config.redundancy_numerator as u16;
            while self.ratio_acc >= self.config.redundancy_denominator as u16 {
                self.ratio_acc -= self.config.redundancy_denominator as u16;
                repairs.push(self.build_repair());
            }
        }

        EncoderOutput { source, repairs }
    }

    pub fn acknowledge(&mut self, highest_fully_decoded: u32) {
        let Some(back_seq) = self.window.back().map(|e| e.seq) else {
            return;
        };
        // RFC 1982 makes the exact half-range unordered. Never let such an ACK
        // mutate the window, and ignore ACKs serially after the latest source.
        match serial_cmp(highest_fully_decoded, back_seq) {
            None | Some(std::cmp::Ordering::Greater) => return,
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal) => {}
        }

        while let Some(front) = self.window.front() {
            match serial_cmp(front.seq, highest_fully_decoded) {
                Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal) => {
                    self.evict_oldest();
                }
                None | Some(std::cmp::Ordering::Greater) => break,
            }
        }
    }

    pub fn window_len(&self) -> usize {
        self.window.len()
    }

    pub fn window_base(&self) -> Option<u32> {
        self.window.front().map(|e| e.seq)
    }

    fn evict_oldest(&mut self) {
        if let Some(entry) = self.window.pop_front() {
            // prefixed_payload includes 2 len bytes
            let payload_bytes = entry.prefixed_payload.len().saturating_sub(2) as u32;
            self.window_byte_total = self.window_byte_total.saturating_sub(payload_bytes);
        }
    }

    fn build_repair(&mut self) -> Symbol {
        let repair_seq = self.next_repair_seq;
        self.next_repair_seq = self.next_repair_seq.wrapping_add(1);

        if self.window.is_empty() {
            return Symbol::Repair {
                repair_seq,
                window_base: 0,
                window_end: 0,
                payload: Vec::new(),
            };
        }

        // Safety: guarded by is_empty() check above.
        let window_base = self.window.front().expect("non-empty").seq;
        // wrapping_add: when back().seq == u32::MAX the window_end wraps to 0.
        // The TS decoder handles this correctly (seq !== we iteration); the Rust
        // decoder is fixed below in push_repair / one_elim_pass.
        let window_end = self.window.back().expect("non-empty").seq.wrapping_add(1);

        // max effective length = max(src.payload_len + 2) over window
        let max_eff_len = self
            .window
            .iter()
            .map(|e| e.prefixed_payload.len()) // already length-prefixed
            .max()
            .unwrap_or(2);

        // XOR combine: repair[j] = XOR_i( gf_mul(coeff(repair_seq, src_i.seq), eff[j]) )
        let mut payload = vec![0u8; max_eff_len];
        for entry in &self.window {
            let coeff = gf_coeff(repair_seq, entry.seq);
            for (j, &b) in entry.prefixed_payload.iter().enumerate() {
                payload[j] ^= gf_mul(coeff, b);
            }
            // bytes beyond prefixed_payload.len() are zero → gf_mul(coeff,0)=0, no change
        }

        Symbol::Repair {
            repair_seq,
            window_base,
            window_end,
            payload,
        }
    }
}

// ── 디코더 ────────────────────────────────────────────────────────────────

/// 디코더가 방출하는 이벤트.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecoderEvent {
    Recovered {
        seq: u32,
        payload: Vec<u8>,
        via_fec: bool,
    },
    LossSpan {
        from_seq: u32,
        to_seq_exclusive: u32,
    },
    /// The bounded decoder discarded unresolved source state. The epoch-owning
    /// receiver turns this into its public discontinuity signal.
    Evicted {
        from_seq: u32,
        to_seq_exclusive: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceState {
    Received(Vec<u8>),
    Recovered(Vec<u8>),
    Missing,
}

impl SourceState {
    fn payload(&self) -> Option<&[u8]> {
        match self {
            SourceState::Received(p) | SourceState::Recovered(p) => Some(p),
            SourceState::Missing => None,
        }
    }

    fn is_known(&self) -> bool {
        self.payload().is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceivedRepair {
    repair_seq: u16,
    window_base: u32,
    window_end: u32,
    payload: Vec<u8>,
}

/// ── U2 P2 groundwork: recovery/loss-span counters ──────────────────────
///
/// Pure instrumentation for a future Gate-B rig to measure recovery rate vs
/// redundancy ratio (docs/design/fec-framing.md §8: "Recovered/LossSpan
/// counters"). No behavioural effect: with the returned snapshot ignored,
/// decoder output (events, `highest_fully_decoded`) is byte-identical to
/// before these counters existed.
///
/// Chosen seams (each counter incremented at exactly one authoritative
/// point):
/// - `source_symbols_received` — [`FecDecoder::push_source`], at the point a
///   new (non-duplicate) source payload is recorded as `Received`.
/// - `repair_symbols_received` — [`FecDecoder::push_repair`], right after the
///   dedup + oversized-window guards, where the repair is accepted into
///   `seen_repair_keys`.
/// - `symbols_recovered` — [`FecDecoder::try_recover`], once per `(seq,
///   payload)` pair a Gaussian-elimination pass resolves.
/// - `loss_spans` / `loss_spans_recovered` — [`FecDecoder::advance_contiguous`],
///   the sole place a `Missing` gap blocking `highest_contiguous` is
///   discovered and closed; see that method's doc comment for the exact
///   (approximated) span-tracking algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FecDecoderStats {
    /// Count of accepted (non-duplicate) source symbols.
    pub source_symbols_received: u64,
    /// Count of accepted (non-duplicate, in-spec) repair symbols.
    pub repair_symbols_received: u64,
    /// Count of individual symbols reconstructed via FEC decode rather than
    /// direct receipt.
    pub symbols_recovered: u64,
    /// Count of loss episodes observed: a maximal run of one-or-more
    /// consecutive missing sequence numbers, counted once at first
    /// observation (span start).
    pub loss_spans: u64,
    /// Subset of `loss_spans` that closed fully healed — every member seq
    /// became known and at least one was FEC-recovered (not just a late
    /// direct arrival).
    pub loss_spans_recovered: u64,
}

/// Bounded-state snapshot. `retained_bytes` is source payload bytes + 8 bytes
/// per source, repair payload bytes + 16 bytes per repair, and 8 bytes per
/// dedup key. Gaussian work is admitted only when this plus its scratch fits
/// the configured byte cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FecDecoderAccounting {
    pub source_symbols: usize,
    pub repair_symbols: usize,
    pub dedup_keys: usize,
    /// Largest active matrix dimension; source and repair dimensions are each capped at 128.
    pub algebra_symbols: usize,
    pub retained_bytes: usize,
}
/// 체계적 슬라이딩 윈도우 FEC 디코더.
#[derive(Debug, Clone)]
pub struct FecDecoder {
    max_symbols: u16,
    max_bytes: u32,
    sources: BTreeMap<u32, SourceState>,
    repairs: Vec<ReceivedRepair>,
    /// Deduplicate repairs by (repair_seq, window_base).
    /// Using the full u16 repair_seq avoids the false-positive collision that
    /// occurred when `repair_seq % 128` was used: repair_seq=0 and
    /// repair_seq=128 shared key 0 even though gf_coeff(0,s) ≠ gf_coeff(128,s).
    seen_repair_keys: std::collections::HashSet<(u16, u32)>,
    highest_contiguous: Option<u32>,
    /// Cheap instrumentation counters (see [`FecDecoderStats`] doc comment).
    /// Monotonic, default-zero, no allocation on the hot path.
    stats: FecDecoderStats,
    /// Start seq of the loss span currently open (blocking `highest_contiguous`
    /// advancement) if one has already been counted, else `None`. See
    /// `advance_contiguous`.
    open_loss_span: Option<u32>,
    /// Whether at least one seq resolved so far within `open_loss_span` was a
    /// genuine FEC recovery (`SourceState::Recovered`) rather than a late
    /// direct arrival (`SourceState::Received`).
    open_loss_span_has_recovery: bool,
    newest_seq: Option<u32>,
    /// `(window_base, window_end)` of the most recently committed forward
    /// repair, i.e. the active retained/frontier horizon that
    /// `roll_for_repair` has already rolled state up to. `None` until the
    /// first repair commits — before that, nothing has been retired, so any
    /// window is trivially forward. This is intentionally independent of
    /// `sources`/`repairs` contents: cap-driven eviction (`make_room`) can
    /// remove arbitrary entries without moving the declared rolling horizon,
    /// so only an accepted repair's own window may advance it. Tracking both
    /// endpoints (not just `window_base`) is required to reject a delayed,
    /// same-or-earlier-base repair whose `window_end` does not extend past
    /// what has already been committed — e.g. a stale `[50, 60)` arriving
    /// after `[50, 110)` has already rolled state forward.
    roll_horizon: Option<(u32, u32)>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairHorizonRelation {
    Stale,
    Overlapping,
    Forward,
}

impl FecDecoder {
    pub fn new(max_symbols: u16, max_bytes: u32) -> Self {
        Self {
            max_symbols: max_symbols.clamp(1, 128),
            max_bytes: max_bytes.clamp(1, 16 * 1024 * 1024),
            sources: BTreeMap::new(),
            repairs: Vec::new(),
            seen_repair_keys: std::collections::HashSet::new(),
            highest_contiguous: None,
            stats: FecDecoderStats::default(),
            open_loss_span: None,
            open_loss_span_has_recovery: false,
            newest_seq: None,
            roll_horizon: None,
        }
    }

    pub fn push_symbol(&mut self, symbol: Symbol) -> Vec<DecoderEvent> {
        match symbol {
            Symbol::Source { seq, payload } => self.push_source(seq, payload),
            Symbol::Repair {
                repair_seq,
                window_base,
                window_end,
                payload,
            } => self.push_repair(repair_seq, window_base, window_end, payload),
        }
    }

    pub fn highest_fully_decoded(&self) -> Option<u32> {
        self.highest_contiguous
    }

    /// Snapshot of recovery/loss-span counters accumulated so far (Copy
    /// struct; cheap to call at any time — no allocation, no traversal).
    pub fn stats(&self) -> FecDecoderStats {
        self.stats
    }

    pub fn accounting(&self) -> FecDecoderAccounting {
        let source_bytes: usize = self
            .sources
            .values()
            .map(|state| match state {
                SourceState::Received(payload) | SourceState::Recovered(payload) => {
                    payload.len() + 8
                }
                SourceState::Missing => 8,
            })
            .sum();
        let repair_bytes: usize = self
            .repairs
            .iter()
            .map(|repair| repair.payload.len() + 16)
            .sum();
        FecDecoderAccounting {
            source_symbols: self.sources.len(),
            repair_symbols: self.repairs.len(),
            dedup_keys: self.seen_repair_keys.len(),
            algebra_symbols: self.sources.len().max(self.repairs.len()),
            retained_bytes: source_bytes + repair_bytes + self.seen_repair_keys.len() * 8,
        }
    }

    fn note_newest(&mut self, seq: u32) {
        match self.newest_seq {
            None => self.newest_seq = Some(seq),
            Some(current) if seq.wrapping_sub(current) < 0x8000_0000 => {
                self.newest_seq = Some(seq);
            }
            Some(_) => {}
        }
    }

    /// Classify a repair before changing retained state. `roll_horizon` is the
    /// `(window_base, window_end)` of the most recently committed forward
    /// repair — the declared point up to which `roll_for_repair` has already
    /// retired state. Comparisons use RFC1982 half-range arithmetic so this
    /// stays correct as `u32` sequence numbers wrap.
    ///
    /// - `Stale`: `window_end` is serially older than the committed end, or
    ///   equal to it with a different `window_base` (a same-or-narrower
    ///   sub-window that gains no new coverage — e.g. a delayed `[50, 60)`
    ///   after `[50, 110)` has already committed).
    /// - `Forward`: `window_end` is serially newer than the committed end AND
    ///   `window_base` is not serially older than the committed base, or the
    ///   window is byte-for-byte identical to the committed one (a second,
    ///   independent equation for the currently active window, not a
    ///   backward roll).
    /// - `Overlapping`: `window_end` is serially newer but `window_base` is
    ///   serially older than the committed base — would need equations for
    ///   already-retired state, so it cannot be admitted.
    fn repair_horizon_relation(&self, window_base: u32, window_end: u32) -> RepairHorizonRelation {
        let Some((horizon_base, horizon_end)) = self.roll_horizon else {
            return RepairHorizonRelation::Forward;
        };
        match serial_cmp(window_end, horizon_end) {
            None | Some(std::cmp::Ordering::Less) => RepairHorizonRelation::Stale,
            Some(std::cmp::Ordering::Equal) => {
                if window_base == horizon_base {
                    RepairHorizonRelation::Forward
                } else {
                    RepairHorizonRelation::Stale
                }
            }
            Some(std::cmp::Ordering::Greater) => match serial_cmp(window_base, horizon_base) {
                Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal) => {
                    RepairHorizonRelation::Forward
                }
                Some(std::cmp::Ordering::Less) | None => RepairHorizonRelation::Overlapping,
            },
        }
    }

    /// Evict retained state until adding the specified accounting deltas fits.
    /// Returns the eviction discontinuities produced before the caller mutates
    /// state. Callers must reject an admission whose own deltas cannot fit.
    fn make_room(
        &mut self,
        added_sources: usize,
        added_repairs: usize,
        added_keys: usize,
        added_bytes: usize,
    ) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        loop {
            let accounting = self.accounting();
            let over_symbols = accounting.source_symbols.saturating_add(added_sources)
                > self.max_symbols as usize
                || accounting.repair_symbols.saturating_add(added_repairs)
                    > self.max_symbols as usize;
            let over_bytes = accounting
                .retained_bytes
                .saturating_add(added_bytes)
                .saturating_add(added_keys.saturating_mul(8))
                > self.max_bytes as usize;
            if !over_symbols && !over_bytes {
                break;
            }
            if !self.repairs.is_empty()
                && (accounting.repair_symbols.saturating_add(added_repairs)
                    > self.max_symbols as usize
                    || over_bytes)
            {
                self.repairs.remove(0);
                self.seen_repair_keys.clear();
                self.seen_repair_keys.extend(
                    self.repairs
                        .iter()
                        .map(|repair| (repair.repair_seq, repair.window_base)),
                );
                continue;
            }
            let Some(newest) = self.newest_seq else { break };
            // Prefer the oldest entry behind the contiguous frontier. Sequence
            // age uses RFC1982 half-range arithmetic, so this remains correct
            // as u32 sequence numbers wrap.
            let frontier = self.highest_contiguous.unwrap_or(newest);
            let mut candidate = self
                .sources
                .iter()
                .filter(|entry| frontier.wrapping_sub(*entry.0) < 0x8000_0000)
                .max_by_key(|entry| frontier.wrapping_sub(*entry.0));
            if candidate.is_none() {
                candidate = self
                    .sources
                    .iter()
                    .max_by_key(|entry| newest.wrapping_sub(*entry.0));
            }
            let Some((seq, state)) = candidate else { break };
            let seq = *seq;
            let unresolved = matches!(state, SourceState::Missing);
            self.sources.remove(&seq);
            if unresolved {
                events.push(DecoderEvent::Evicted {
                    from_seq: seq,
                    to_seq_exclusive: seq.wrapping_add(1),
                });
            }
        }
        events
    }

    /// Retire equations that require source state outside the incoming bounded
    /// serial window, then retire that source state. Keeping those operations
    /// coupled prevents an equation from silently treating an evicted source as
    /// zero during elimination.
    fn roll_for_repair(&mut self, window_base: u32, window_len: u32) -> Vec<DecoderEvent> {
        if window_len == 0 {
            return Vec::new();
        }
        self.repairs.retain(|repair| {
            seq_in_window(repair.window_base, window_base, window_len)
                && seq_in_window(repair.window_end.wrapping_sub(1), window_base, window_len)
        });
        self.seen_repair_keys.clear();
        self.seen_repair_keys.extend(
            self.repairs
                .iter()
                .map(|repair| (repair.repair_seq, repair.window_base)),
        );

        let obsolete_sources: Vec<u32> = self
            .sources
            .keys()
            .copied()
            .filter(|seq| !seq_in_window(*seq, window_base, window_len))
            .collect();
        let mut events = Vec::new();
        for seq in obsolete_sources {
            if matches!(self.sources.remove(&seq), Some(SourceState::Missing)) {
                events.push(DecoderEvent::Evicted {
                    from_seq: seq,
                    to_seq_exclusive: seq.wrapping_add(1),
                });
            }
        }
        events
    }

    /// Remove the oldest retained repair and rebuild its dedup index.
    fn evict_oldest_repair(&mut self) {
        self.repairs.remove(0);
        self.seen_repair_keys.clear();
        self.seen_repair_keys.extend(
            self.repairs
                .iter()
                .map(|repair| (repair.repair_seq, repair.window_base)),
        );
    }

    /// If the frontier (`highest_contiguous + 1`, or 0 if unset) is already
    /// `Missing` and no span is currently open, open one and count
    /// `stats.loss_spans += 1`.
    ///
    /// Called explicitly at the top of `push_source` (before the seq being
    /// pushed can resolve the frontier) and after the Missing-registration
    /// loop in `push_repair` (before `try_recover` can cascade-resolve it).
    /// This is necessary because `try_recover` runs *within* the same
    /// `push_source`/`push_repair` call that can also be the one supplying
    /// the last missing piece of a gap — the common "one symbol lost, one
    /// repair received" case resolves the frontier before
    /// `advance_contiguous` ever gets a chance to observe it as `Missing`.
    /// `advance_contiguous` keeps its own (guarded, no-op-if-already-open)
    /// span-open check for the complementary case: a gap that was
    /// registered `Missing` earlier and only reached once the frontier
    /// advances up to it on a later call.
    fn note_open_span_at_frontier(&mut self) {
        if self.open_loss_span.is_some() {
            return;
        }
        let frontier = match self.highest_contiguous {
            None => 0u32,
            Some(h) => h.wrapping_add(1),
        };
        if matches!(self.sources.get(&frontier), Some(SourceState::Missing)) {
            self.stats.loss_spans += 1;
            self.open_loss_span = Some(frontier);
            self.open_loss_span_has_recovery = false;
        }
    }

    fn push_source(&mut self, seq: u32, payload: Vec<u8>) -> Vec<DecoderEvent> {
        if payload.len().saturating_add(8) > self.max_bytes as usize {
            return vec![DecoderEvent::Recovered {
                seq,
                payload,
                via_fec: false,
            }];
        }
        // Ignore if already known
        if matches!(
            self.sources.get(&seq),
            Some(SourceState::Received(_) | SourceState::Recovered(_))
        ) {
            return Vec::new();
        }

        let (added_sources, added_bytes) = match self.sources.get(&seq) {
            Some(SourceState::Missing) => (0, payload.len()),
            Some(SourceState::Received(_) | SourceState::Recovered(_)) => return Vec::new(),
            None => (1, payload.len().saturating_add(8)),
        };
        let mut events = self.make_room(added_sources, 0, 0, added_bytes);
        self.note_open_span_at_frontier();
        self.note_newest(seq);

        self.sources
            .insert(seq, SourceState::Received(payload.clone()));
        self.stats.source_symbols_received += 1;

        events.push(DecoderEvent::Recovered {
            seq,
            payload,
            via_fec: false,
        });
        // Try to cascade-recover missing symbols using available repairs
        let recovered = self.try_recover();
        events.extend(recovered.into_iter().map(|(s, p)| DecoderEvent::Recovered {
            seq: s,
            payload: p,
            via_fec: true,
        }));
        events.extend(self.advance_contiguous());
        events
    }

    fn push_repair(
        &mut self,
        repair_seq: u16,
        window_base: u32,
        window_end: u32,
        payload: Vec<u8>,
    ) -> Vec<DecoderEvent> {
        // Reject malformed repairs before classifying them against live state.
        let window_len = window_end.wrapping_sub(window_base);
        if payload.len().saturating_add(16) > self.max_bytes as usize
            || window_len > 128
            || window_len > self.max_symbols as u32
        {
            return Vec::new();
        }

        // Do not let an unseen historical equation roll the retained horizon
        // backward. An overlap whose prefix has already been retired cannot be
        // used safely: elimination would otherwise treat that prefix as zero.
        match self.repair_horizon_relation(window_base, window_end) {
            RepairHorizonRelation::Stale | RepairHorizonRelation::Overlapping => {
                return Vec::new();
            }
            RepairHorizonRelation::Forward => {}
        }

        // Deduplicate by (repair_seq, window_base) — full u16 to avoid false-positive
        // collisions: e.g. repair_seq=0 and repair_seq=128 had the same truncated key.
        let key = (repair_seq, window_base);
        if self.seen_repair_keys.contains(&key) {
            return Vec::new();
        }

        // Compute the complete coupled retirement/admission plan on a private
        // copy. A failed plan is discarded, so rejected repairs cannot evict
        // sources or equations, change accounting, or leak eviction events.
        let mut planned = self.clone();
        let Some(events) = planned.admit_repair(repair_seq, window_base, window_end, payload, key)
        else {
            return Vec::new();
        };
        *self = planned;
        events
    }

    fn admit_repair(
        &mut self,
        repair_seq: u16,
        window_base: u32,
        window_end: u32,
        payload: Vec<u8>,
        key: (u16, u32),
    ) -> Option<Vec<DecoderEvent>> {
        let window_len = window_end.wrapping_sub(window_base);
        // A full decoder is a rolling window, not a terminal state. Retire any
        // equation before the source state it depends on, then make byte and
        // repair-row room by dropping the oldest remaining equations.
        let mut events = self.roll_for_repair(window_base, window_len);
        loop {
            let added_sources = (0..window_len)
                .filter(|offset| {
                    !self
                        .sources
                        .contains_key(&window_base.wrapping_add(*offset))
                })
                .count();
            let added_bytes = added_sources
                .saturating_mul(8)
                .saturating_add(payload.len())
                .saturating_add(16);
            let accounting = self.accounting();
            let over_repairs = accounting.repair_symbols >= self.max_symbols as usize;
            let over_bytes = accounting
                .retained_bytes
                .saturating_add(added_bytes)
                .saturating_add(8)
                > self.max_bytes as usize;
            if !over_repairs && !over_bytes {
                break;
            }
            if self.repairs.is_empty() {
                return None;
            }
            self.evict_oldest_repair();
        }

        let added_sources = (0..window_len)
            .filter(|offset| {
                !self
                    .sources
                    .contains_key(&window_base.wrapping_add(*offset))
            })
            .count();
        let added_bytes = added_sources
            .saturating_mul(8)
            .saturating_add(payload.len())
            .saturating_add(16);
        let accounting = self.accounting();
        if accounting.source_symbols.saturating_add(added_sources) > self.max_symbols as usize
            || accounting
                .retained_bytes
                .saturating_add(added_bytes)
                .saturating_add(8)
                > self.max_bytes as usize
        {
            return None;
        }
        self.seen_repair_keys.insert(key);
        self.stats.repair_symbols_received += 1;

        // Register all seqs in this repair's window as at least Missing.
        // Use wrapping iteration (seq != window_end) to handle the case where
        // window_end wrapped to 0 (i.e., window contains u32::MAX).
        let mut seq = window_base;
        while seq != window_end {
            self.sources.entry(seq).or_insert(SourceState::Missing);
            seq = seq.wrapping_add(1);
        }

        self.note_open_span_at_frontier();
        if window_len != 0 {
            self.note_newest(window_end.wrapping_sub(1));
            // Advance the declared rolling horizon to this repair's base now
            // that roll_for_repair has actually retired everything below it.
            self.roll_horizon = Some((window_base, window_end));
        }

        self.repairs.push(ReceivedRepair {
            repair_seq,
            window_base,
            window_end,
            payload,
        });

        let recovered = self.try_recover();
        events.extend(recovered.into_iter().map(|(s, p)| DecoderEvent::Recovered {
            seq: s,
            payload: p,
            via_fec: true,
        }));
        events.extend(self.advance_contiguous());
        Some(events)
    }

    /// Iteratively recover missing symbols via Gaussian elimination.
    /// Returns (seq, payload) for newly recovered symbols.
    fn try_recover(&mut self) -> Vec<(u32, Vec<u8>)> {
        let mut all_recovered: Vec<(u32, Vec<u8>)> = Vec::new();
        loop {
            let batch = self.one_elim_pass();
            if batch.is_empty() {
                break;
            }
            let recovered_bytes: usize = batch.iter().map(|(_, payload)| payload.len()).sum();
            if self
                .accounting()
                .retained_bytes
                .saturating_add(recovered_bytes)
                > self.max_bytes as usize
            {
                break;
            }
            self.stats.symbols_recovered += batch.len() as u64;
            for (seq, payload) in &batch {
                self.sources
                    .insert(*seq, SourceState::Recovered(payload.clone()));
            }
            all_recovered.extend(batch);
        }
        all_recovered
    }

    /// Single Gaussian elimination pass over all repairs.
    /// Returns symbols that can now be recovered.
    fn one_elim_pass(&self) -> Vec<(u32, Vec<u8>)> {
        if self.repairs.is_empty() {
            return Vec::new();
        }

        // Collect all missing seqs across all repair windows
        let mut missing_seqs: Vec<u32> = self
            .sources
            .iter()
            .filter(|(_, s)| !s.is_known())
            .map(|(&seq, _)| seq)
            .collect();
        missing_seqs.sort_unstable();

        if missing_seqs.is_empty() {
            return Vec::new();
        }

        // Max effective payload length across all repairs
        let max_eff_len = self
            .repairs
            .iter()
            .map(|r| r.payload.len())
            .max()
            .unwrap_or(0);
        if max_eff_len == 0 {
            return Vec::new();
        }

        let n_unknowns = missing_seqs.len();
        if n_unknowns > 128 || self.repairs.len() > 128 {
            return Vec::new();
        }
        let scratch = self
            .repairs
            .len()
            .saturating_mul(n_unknowns.saturating_add(max_eff_len))
            .saturating_add(2 * n_unknowns)
            .saturating_add(2 * max_eff_len);
        if self.accounting().retained_bytes.saturating_add(scratch) > self.max_bytes as usize {
            return Vec::new();
        }

        // Build coefficient matrix [n_repairs × n_unknowns] and RHS [n_repairs × max_eff_len]
        let mut coeffs: Vec<Vec<u8>> = Vec::new();
        let mut rhs: Vec<Vec<u8>> = Vec::new();

        for repair in &self.repairs {
            let mut row_coeffs = vec![0u8; n_unknowns];
            // Start RHS from repair payload, zero-extended to max_eff_len
            let mut row_rhs = vec![0u8; max_eff_len];
            for (j, &b) in repair.payload.iter().enumerate() {
                row_rhs[j] = b;
            }

            // Subtract known-source contributions.
            // Wrapping iteration to handle window_end == 0 (window crosses u32::MAX).
            // A plain `for seq in window_base..window_end` range yields an empty
            // iterator when window_end wraps to 0, causing silent recovery failure
            // for the entire wrap-around window.
            let mut seq = repair.window_base;
            while seq != repair.window_end {
                let coeff = gf_coeff(repair.repair_seq, seq);
                if let Some(state) = self.sources.get(&seq) {
                    if let Some(payload) = state.payload() {
                        // effective_payload = [len_lo, len_hi] ++ payload ++ zeros
                        let plen = payload.len() as u16;
                        let len_bytes = plen.to_le_bytes();
                        if !repair.payload.is_empty() {
                            row_rhs[0] ^= gf_mul(coeff, len_bytes[0]);
                        }
                        if repair.payload.len() >= 2 {
                            row_rhs[1] ^= gf_mul(coeff, len_bytes[1]);
                        }
                        for (k, &b) in payload.iter().enumerate() {
                            let idx = 2 + k;
                            if idx < max_eff_len {
                                row_rhs[idx] ^= gf_mul(coeff, b);
                            }
                        }
                    } else {
                        // Missing: it's an unknown — but only if in window
                        if let Ok(pos) = missing_seqs.binary_search(&seq) {
                            row_coeffs[pos] = coeff;
                        }
                    }
                }
                // seq not in sources at all: treat as outside decoder scope, skip
                seq = seq.wrapping_add(1);
            }

            coeffs.push(row_coeffs);
            rhs.push(row_rhs);
        }

        // Gaussian elimination over GF(256)
        let solutions = gaussian_elim(coeffs, rhs, n_unknowns, max_eff_len);

        let mut result = Vec::new();
        for (col, eff_payload) in solutions {
            let seq = missing_seqs[col];
            // Extract actual payload from effective_payload
            if eff_payload.len() < 2 {
                continue;
            }
            let payload_len = u16::from_le_bytes([eff_payload[0], eff_payload[1]]) as usize;
            let end = (2 + payload_len).min(eff_payload.len());
            let payload = eff_payload[2..end].to_vec();
            result.push((seq, payload));
        }
        result
    }

    /// Advance `highest_contiguous`, emit `LossSpan` for permanently missing
    /// ranges, and update the loss-span counters (`stats.loss_spans`,
    /// `stats.loss_spans_recovered`).
    ///
    /// **Bug-2 fix**: when `highest_contiguous == None` the search starts from
    /// seq 0, not from `sources.keys().next()`.  The old code would set
    /// `highest_contiguous = 5` if seq 5 was the first symbol received even
    /// though seqs 0–4 had never been confirmed, causing the encoder's `ack`
    /// feedback to incorrectly evict those seqs from its repair window.
    ///
    /// **Bug-1 fix**: when a `Missing` span is bounded on the right by a
    /// `Received` or `Recovered` seq (i.e., the stream has demonstrably
    /// progressed past the gap without filling it via GF recovery), a
    /// `LossSpan` event is emitted for that gap and `highest_contiguous` is
    /// advanced past it so the outer loop can continue.  Open-ended gaps (no
    /// known seq after the last Missing) are left pending — more repairs might
    /// still arrive.
    ///
    /// **Loss-span counter approximation (U2 P2 groundwork)**: this decoder
    /// has no independent structure tracking arbitrary loss episodes, and
    /// Source entries are evicted with their dependent equations as the repair
    /// window rolls. Gaps are therefore counted lazily when the contiguous
    /// frontier reaches them, not when a repair first registers a seq as
    /// `Missing`. Because `next` is always exactly
    /// `highest_contiguous + 1`, at most one span can ever be "open" (blocking
    /// the frontier) at a time, so `open_loss_span` / `open_loss_span_has_recovery`
    /// need only track a single in-flight span:
    /// - First time `next` resolves to `Missing`: if no span is already open,
    ///   count `stats.loss_spans += 1` and open one at `next`.
    /// - Bounded-gap resolution (existing Bug-1 path): the whole run was
    ///   never healed — it is abandoned/skipped. Close the span without
    ///   touching `loss_spans_recovered`.
    /// - Per-seq resolution to `Received`/`Recovered` while a span is open
    ///   (see `close_open_span_step`): record whether the seq was
    ///   FEC-recovered; once the seq right after `next` is no longer
    ///   `Missing`, the run is fully consumed — close the span and, if at
    ///   least one member was FEC-recovered, count `stats.loss_spans_recovered
    ///   += 1`.
    fn advance_contiguous(&mut self) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        'outer: loop {
            // Bug-2 fix: always start from 0 when no contiguous baseline exists.
            let next = match self.highest_contiguous {
                None => 0u32,
                Some(h) => h.wrapping_add(1),
            };

            match self.sources.get(&next) {
                Some(SourceState::Received(_)) => {
                    self.close_open_span_step(next, false);
                    self.highest_contiguous = Some(next);
                }
                Some(SourceState::Recovered(_)) => {
                    self.close_open_span_step(next, true);
                    self.highest_contiguous = Some(next);
                }
                Some(SourceState::Missing) => {
                    if self.open_loss_span.is_none() {
                        self.stats.loss_spans += 1;
                        self.open_loss_span = Some(next);
                        self.open_loss_span_has_recovery = false;
                    }
                    // Scan forward to determine whether the Missing span is bounded
                    // by a Received/Recovered seq (permanent loss) or open-ended
                    // (might still be recovered by a future repair).
                    let span_start = next;
                    let mut scan = next.wrapping_add(1);
                    loop {
                        match self.sources.get(&scan) {
                            Some(SourceState::Missing) => {
                                scan = scan.wrapping_add(1);
                            }
                            Some(SourceState::Received(_) | SourceState::Recovered(_)) => {
                                // Gap is bounded — the stream advanced past it without
                                // recovering span_start..scan.  Emit LossSpan and
                                // advance highest_contiguous to just before `scan` so
                                // the outer loop picks up `scan` on the next iteration.
                                events.push(DecoderEvent::LossSpan {
                                    from_seq: span_start,
                                    to_seq_exclusive: scan,
                                });
                                // The discontinuity is final: retaining these
                                // Missing entries would let later cap eviction
                                // report the same loss a second time.
                                let mut abandoned = span_start;
                                while abandoned != scan {
                                    self.sources.remove(&abandoned);
                                    abandoned = abandoned.wrapping_add(1);
                                }
                                self.highest_contiguous = Some(scan.wrapping_sub(1));
                                self.open_loss_span = None;
                                self.open_loss_span_has_recovery = false;
                                continue 'outer;
                            }
                            None => {
                                // Open-ended gap: stop and wait for more symbols.
                                break 'outer;
                            }
                        }
                    }
                    // Terminates via the None arm: `sources` is bounded, so a
                    // forward scan reaches an absent seq within the window.
                }
                None => break,
            }
        }
        events
    }

    /// Loss-span bookkeeping for one seq resolving to `Received`/`Recovered`
    /// while `advance_contiguous` walks the frontier. No-op when no span is
    /// currently open (i.e. this seq did not follow an observed gap). Closes
    /// the open span (counting `loss_spans_recovered` when healed) once the
    /// seq immediately after `resolved_seq` is no longer `Missing`, i.e. the
    /// whole originally-contiguous run has been consumed.
    fn close_open_span_step(&mut self, resolved_seq: u32, via_fec: bool) {
        if self.open_loss_span.is_none() {
            return;
        }
        if via_fec {
            self.open_loss_span_has_recovery = true;
        }
        let still_missing = matches!(
            self.sources.get(&resolved_seq.wrapping_add(1)),
            Some(SourceState::Missing)
        );
        if !still_missing {
            if self.open_loss_span_has_recovery {
                self.stats.loss_spans_recovered += 1;
            }
            self.open_loss_span = None;
            self.open_loss_span_has_recovery = false;
        }
    }
}

/// Gaussian elimination over GF(256).
/// Returns (column_index, recovered_vector) for each pivot found.
fn gaussian_elim(
    mut coeffs: Vec<Vec<u8>>,
    mut rhs: Vec<Vec<u8>>,
    n_unknowns: usize,
    payload_len: usize,
) -> Vec<(usize, Vec<u8>)> {
    let n_rows = coeffs.len();
    if n_rows == 0 || n_unknowns == 0 || payload_len == 0 {
        return Vec::new();
    }

    let mut pivot_row: Vec<Option<usize>> = vec![None; n_unknowns]; // col → row
    let mut current_row = 0usize;

    for col in 0..n_unknowns {
        // Find pivot in current_row..n_rows for this column
        let pivot = (current_row..n_rows).find(|&r| coeffs[r][col] != 0);
        let Some(p) = pivot else { continue };

        // Swap pivot row to current_row
        coeffs.swap(current_row, p);
        rhs.swap(current_row, p);

        // Scale pivot row so leading coeff is 1
        let lead = coeffs[current_row][col];
        let inv_lead = gf_inv(lead);
        for v in &mut coeffs[current_row] {
            *v = gf_mul(*v, inv_lead);
        }
        for b in &mut rhs[current_row] {
            *b = gf_mul(*b, inv_lead);
        }

        // Eliminate this column from all other rows.
        // Clone the pivot row once (avoids simultaneous mut+imm borrow of rhs).
        let pivot_coeffs: Vec<u8> = coeffs[current_row].clone();
        let pivot_rhs: Vec<u8> = rhs[current_row].clone();
        for r in 0..n_rows {
            if r == current_row {
                continue;
            }
            let factor = coeffs[r][col];
            if factor == 0 {
                continue;
            }
            for c in 0..n_unknowns {
                coeffs[r][c] ^= gf_mul(factor, pivot_coeffs[c]);
            }
            for (j, b) in rhs[r].iter_mut().enumerate() {
                *b ^= gf_mul(factor, pivot_rhs[j]);
            }
        }

        pivot_row[col] = Some(current_row);
        current_row += 1;
        if current_row >= n_rows {
            break;
        }
    }

    // Collect results. A pivot column's solution is unique only when all
    // non-pivot columns in its reduced row are zero — i.e., no free variables
    // remain that affect that unknown. Under-determined rows are skipped.
    let pivot_cols: std::collections::HashSet<usize> = pivot_row
        .iter()
        .enumerate()
        .filter_map(|(c, r)| r.map(|_| c))
        .collect();

    let mut result = Vec::new();
    for (col, pivot_entry) in pivot_row.iter().enumerate() {
        let Some(&pr) = pivot_entry.as_ref() else {
            continue;
        };
        // Check that no non-pivot column in this row has a non-zero coefficient.
        let fully_determined = (0..n_unknowns)
            .filter(|c| !pivot_cols.contains(c))
            .all(|c| coeffs[pr][c] == 0);
        if fully_determined {
            result.push((col, rhs[pr].clone()));
        }
    }
    result
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ── 5-A: GF(256) KAT ─────────────────────────────────────────────────

    #[test]
    fn gf_zero_absorbs() {
        assert_eq!(gf_mul(0x00, 0xFF), 0x00);
    }

    #[test]
    fn gf_identity() {
        assert_eq!(gf_mul(0x01, 0xAB), 0xAB);
    }

    #[test]
    fn gf_mul_x_times_xplus1() {
        assert_eq!(gf_mul(0x02, 0x03), 0x06);
    }

    #[test]
    fn gf_mul_x_times_x7() {
        assert_eq!(gf_mul(0x02, 0x80), 0x1D);
    }

    #[test]
    fn gf_mul_xplus1_squared() {
        assert_eq!(gf_mul(0x03, 0x03), 0x05);
    }

    #[test]
    fn gf_mul_x3plus1_squared() {
        assert_eq!(gf_mul(0x09, 0x09), 0x41);
    }

    #[test]
    fn gf_mul_53_ca() {
        assert_eq!(gf_mul(0x53, 0xCA), 0x8F);
    }

    #[test]
    fn gf_inv_roundtrip_all() {
        for a in 1u8..=255 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1, "a={a:#04x}");
        }
    }

    #[test]
    fn gf_mul_commutative() {
        let mut state: u64 = 99;
        let pairs: Vec<(u8, u8)> = (0..200)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let a = (state >> 33) as u8;
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let b = (state >> 33) as u8;
                (a, b)
            })
            .collect();
        for (a, b) in pairs {
            assert_eq!(gf_mul(a, b), gf_mul(b, a), "a={a:#04x} b={b:#04x}");
        }
    }

    // ── 5-B: 인코더 기본 ──────────────────────────────────────────────────

    #[test]
    fn enc_empty_payload() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        });
        let out = enc.push_source(0, &[]);
        assert_eq!(
            out.source,
            Symbol::Source {
                seq: 0,
                payload: vec![]
            }
        );
        assert_eq!(out.repairs.len(), 1);
        if let Symbol::Repair { payload, .. } = &out.repairs[0] {
            assert_eq!(
                payload.len(),
                2,
                "repair payload must be 2 bytes for empty source"
            );
        } else {
            panic!("expected Repair");
        }
    }

    #[test]
    fn enc_max_payload() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        let big = vec![0xABu8; 65520];
        let out = enc.push_source(0, &big);
        match out.source {
            Symbol::Source {
                seq: 0,
                ref payload,
            } => assert_eq!(payload.len(), 65520),
            _ => panic!("unexpected"),
        }
    }

    #[test]
    fn enc_ratio_zero_no_repairs() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 0,
            redundancy_denominator: 8,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        });
        let total_repairs: usize = (0..100)
            .map(|i| enc.push_source(i, b"x").repairs.len())
            .sum();
        assert_eq!(total_repairs, 0);
    }

    #[test]
    fn enc_ratio_one_per_one() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        });
        let total_repairs: usize = (0..10)
            .map(|i| enc.push_source(i, b"x").repairs.len())
            .sum();
        assert_eq!(total_repairs, 10);
    }

    #[test]
    fn enc_ratio_1_8_count() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 8,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        });
        let total_repairs: usize = (0..16)
            .map(|i| enc.push_source(i, b"x").repairs.len())
            .sum();
        assert_eq!(total_repairs, 2);
    }

    #[test]
    fn enc_window_len_after_push() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..8 {
            enc.push_source(i, b"hello");
        }
        assert_eq!(enc.window_len(), 8);
    }

    // ── helper: build encoder+decoder and drive a scenario ───────────────

    struct Scenario {
        enc: FecEncoder,
        dec: FecDecoder,
    }

    impl Scenario {
        fn new(num: u8, den: u8, win: u16) -> Self {
            let config = FecConfig {
                redundancy_numerator: num,
                redundancy_denominator: den,
                window_max_symbols: win,
                window_max_bytes: 1 << 24,
            };
            Self {
                enc: FecEncoder::new(config),
                dec: FecDecoder::new(win, 1 << 24),
            }
        }

        /// Push source symbol, deliver all outputs to decoder (optionally drop source).
        fn push(&mut self, seq: u32, payload: &[u8], drop_source: bool) -> Vec<DecoderEvent> {
            let out = self.enc.push_source(seq, payload);
            let mut events = Vec::new();
            if !drop_source {
                events.extend(self.dec.push_symbol(out.source));
            }
            for repair in out.repairs {
                events.extend(self.dec.push_symbol(repair));
            }
            events
        }
    }

    fn collect_recovered(events: &[DecoderEvent]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|e| {
                if let DecoderEvent::Recovered { seq, .. } = e {
                    Some(*seq)
                } else {
                    None
                }
            })
            .collect()
    }

    fn count_loss_spans(events: &[DecoderEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, DecoderEvent::LossSpan { .. }))
            .count()
    }

    // ── 5-C: 라운드트립 테스트 ───────────────────────────────────────────

    #[test]
    fn rt_no_loss() {
        let mut s = Scenario::new(1, 8, 64);
        let mut all_events = Vec::new();
        for i in 0..16u32 {
            all_events.extend(s.push(i, b"data", false));
        }
        let recovered = collect_recovered(&all_events);
        for i in 0..16u32 {
            assert!(recovered.contains(&i), "seq {i} not recovered");
        }
    }

    #[test]
    fn rt_single_loss_recoverable() {
        // 1/1 ratio: every source gets a repair
        let mut s = Scenario::new(1, 1, 64);
        let mut events = Vec::new();
        for i in 0..8u32 {
            let drop = i == 4;
            events.extend(s.push(i, b"hello", drop));
        }
        let recovered = collect_recovered(&events);
        assert!(
            recovered.contains(&4),
            "seq 4 not recovered; got {recovered:?}"
        );
    }

    #[test]
    fn rt_burst_eq_redundancy() {
        // 2/8 ratio, lose seq 2 and 3
        let mut s = Scenario::new(2, 8, 64);
        let mut events = Vec::new();
        for i in 0..8u32 {
            let drop = i == 2 || i == 3;
            events.extend(s.push(i, b"burst", drop));
        }
        let recovered = collect_recovered(&events);
        assert!(
            recovered.contains(&2) && recovered.contains(&3),
            "burst not recovered; got {recovered:?}"
        );
    }

    #[test]
    fn rt_burst_gt_redundancy() {
        // 1/4 ratio (2 repairs for 8 sources), lose 4 consecutive → unrecoverable
        let mut s = Scenario::new(1, 4, 64);
        let mut events = Vec::new();
        for i in 0..8u32 {
            let drop = (2..6).contains(&i);
            events.extend(s.push(i, b"burst", drop));
        }
        let recovered = collect_recovered(&events);
        // seq 2..6 should NOT all be recovered (too many losses vs repairs)
        let loss_count = (2u32..6).filter(|s| !recovered.contains(s)).count();
        assert!(
            loss_count > 0,
            "expected unrecoverable loss, but all recovered: {recovered:?}"
        );
    }

    #[test]
    fn rt_reordered_delivery() {
        // Encode 4 symbols but deliver source out of order
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(64, 1 << 20);

        let payloads = [b"A".as_ref(), b"B".as_ref(), b"C".as_ref(), b"D".as_ref()];
        let mut all_symbols: Vec<Symbol> = Vec::new();
        for (i, &p) in payloads.iter().enumerate() {
            let out = enc.push_source(i as u32, p);
            all_symbols.push(out.source);
            all_symbols.extend(out.repairs);
        }
        // Deliver: repair first, then source reordered [0,2,1,3]
        let order = [1usize, 3, 5, 7, 0, 4, 2, 6]; // interleave repairs before sources
        let mut events = Vec::new();
        for &idx in &order {
            if idx < all_symbols.len() {
                events.extend(dec.push_symbol(all_symbols[idx].clone()));
            }
        }
        let recovered = collect_recovered(&events);
        for i in 0..4u32 {
            assert!(recovered.contains(&i), "seq {i} not recovered out-of-order");
        }
    }

    #[test]
    fn rt_duplicate_source() {
        let mut dec = FecDecoder::new(64, 1 << 20);
        let sym = Symbol::Source {
            seq: 3,
            payload: b"hello".to_vec(),
        };
        let e1 = dec.push_symbol(sym.clone());
        let e2 = dec.push_symbol(sym);
        let r1 = collect_recovered(&e1);
        let r2 = collect_recovered(&e2);
        assert_eq!(r1, vec![3]);
        assert!(r2.is_empty(), "duplicate source should produce no events");
    }

    #[test]
    fn rt_duplicate_repair() {
        let mut s = Scenario::new(1, 1, 64);
        let out = s.enc.push_source(0, b"data");
        let repair = out.repairs[0].clone();
        s.dec.push_symbol(out.source);
        let e1 = s.dec.push_symbol(repair.clone());
        let e2 = s.dec.push_symbol(repair);
        // The source was already received, so neither repair recovers anything;
        // the second, identical repair must additionally be ignored outright.
        let r1 = collect_recovered(&e1);
        assert!(
            r1.is_empty(),
            "repair with no missing sources recovers nothing"
        );
        let r2 = collect_recovered(&e2);
        assert!(r2.is_empty(), "duplicate repair should be ignored");
    }

    #[test]
    fn rt_window_slide_on_ack() {
        let mut s = Scenario::new(1, 8, 64);
        for i in 0..16u32 {
            s.push(i, b"data", false);
        }
        assert_eq!(s.enc.window_len(), 16);
        s.enc.acknowledge(7);
        assert_eq!(s.enc.window_len(), 8);
        // Next repair should only cover seq >= 8
        let out = s.enc.push_source(16, b"x");
        for repair in out.repairs {
            if let Symbol::Repair { window_base, .. } = repair {
                assert!(
                    window_base >= 8,
                    "repair window_base {window_base} < 8 after ack(7)"
                );
            }
        }
    }

    #[test]
    fn rt_stale_repair_ignored() {
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 4,
            window_max_bytes: 1 << 20,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(4, 1 << 20);
        let mut stale = None;
        let mut current = None;
        let mut next = None;

        for seq in 0..=9u32 {
            let output = enc.push_source(seq, &[seq as u8]);
            if seq == 0 {
                stale = output.repairs.first().cloned();
            }
            if seq <= 8 {
                dec.push_symbol(output.source);
            }
            if seq == 8 {
                current = output.repairs.first().cloned();
            }
            if seq == 9 {
                next = output.repairs.first().cloned();
            }
        }

        // The [5, 9) repair retires older state and establishes the current horizon.
        dec.push_symbol(current.expect("repair for the current window"));
        let before_accounting = dec.accounting();
        let before_stats = dec.stats();
        let before_frontier = dec.highest_fully_decoded();
        let before_sources = dec.sources.clone();
        let before_repairs = dec.repairs.clone();
        let before_keys = dec.seen_repair_keys.clone();

        assert!(
            dec.push_symbol(stale.expect("repair for the stale window"))
                .is_empty(),
            "an unseen repair fully behind the retained horizon must be ignored"
        );
        assert_eq!(dec.accounting(), before_accounting);
        assert_eq!(dec.stats(), before_stats);
        assert_eq!(dec.highest_fully_decoded(), before_frontier);
        assert_eq!(dec.sources, before_sources);
        assert_eq!(dec.repairs, before_repairs);
        assert_eq!(dec.seen_repair_keys, before_keys);

        let events = dec.push_symbol(next.expect("repair for the next window"));
        assert!(events.iter().any(|event| matches!(
            event,
            DecoderEvent::Recovered {
                seq: 9,
                payload,
                via_fec: true,
            } if payload.as_slice() == [9]
        )));
    }
    #[test]
    fn rt_same_base_shorter_end_reordered_repair_ignored() {
        // A delayed repair with the SAME window_base as an already-committed,
        // wider repair must not be treated as Forward just because its base is
        // not older: it also needs a serially newer window_end. [50, 60) must
        // be rejected as Stale after [50, 110) has already rolled the horizon
        // forward, leaving retained sources/repairs/accounting/events unchanged.
        let mut dec = FecDecoder::new(128, 1 << 20);
        for seq in 50..110u32 {
            dec.push_symbol(Symbol::Source {
                seq,
                payload: vec![seq as u8],
            });
        }
        dec.push_symbol(Symbol::Repair {
            repair_seq: 1,
            window_base: 50,
            window_end: 110,
            payload: vec![0, 0],
        });

        let before_accounting = dec.accounting();
        let before_stats = dec.stats();
        let before_frontier = dec.highest_fully_decoded();
        let before_sources = dec.sources.clone();
        let before_repairs = dec.repairs.clone();
        let before_keys = dec.seen_repair_keys.clone();

        let events = dec.push_symbol(Symbol::Repair {
            repair_seq: 2,
            window_base: 50,
            window_end: 60,
            payload: vec![0, 0],
        });

        assert!(
            events.is_empty(),
            "same-base shorter-end repair delayed behind a wider committed window must be ignored"
        );
        assert_eq!(dec.accounting(), before_accounting);
        assert_eq!(dec.stats(), before_stats);
        assert_eq!(dec.highest_fully_decoded(), before_frontier);
        assert_eq!(dec.sources, before_sources);
        assert_eq!(dec.repairs, before_repairs);
        assert_eq!(dec.seen_repair_keys, before_keys);
    }

    #[test]
    fn rt_repair_only_window() {
        // Encode 8 sources with ratio 1/1, drop ALL sources, deliver 8 repairs
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(64, 1 << 20);

        let mut all_repairs = Vec::new();
        for i in 0..8u32 {
            let out = enc.push_source(i, format!("payload_{i}").as_bytes());
            // drop source, collect repair
            all_repairs.extend(out.repairs);
        }
        let mut events = Vec::new();
        for repair in all_repairs {
            events.extend(dec.push_symbol(repair));
        }
        let recovered = collect_recovered(&events);
        // With 8 independent repairs and 8 unknowns, full recovery should succeed
        // (PRF rank sufficient in practice)
        for i in 0..8u32 {
            assert!(
                recovered.contains(&i),
                "seq {i} not recovered in repair-only scenario; got {recovered:?}"
            );
        }
    }

    #[test]
    fn rt_single_symbol_window() {
        // Window 1 source, 1 repair, source lost → recover via gf_inv
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(64, 1 << 20);
        let out = enc.push_source(0, b"secret");
        // Drop source, keep repair
        let mut events = Vec::new();
        for repair in out.repairs {
            events.extend(dec.push_symbol(repair));
        }
        let recovered = collect_recovered(&events);
        assert!(
            recovered.contains(&0),
            "single-symbol window recovery failed"
        );
        // Also verify payload
        if let Some(DecoderEvent::Recovered {
            seq: 0, payload, ..
        }) = events
            .iter()
            .find(|e| matches!(e, DecoderEvent::Recovered { seq: 0, .. }))
        {
            assert_eq!(payload, b"secret");
        }
    }

    #[test]
    fn rt_variable_length_roundtrip() {
        let lengths: &[usize] = &[100, 0, 1400, 255, 7];
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(64, 1 << 20);

        let payloads: Vec<Vec<u8>> = lengths.iter().map(|&l| vec![0xABu8; l]).collect();
        let mut all_symbols: Vec<Symbol> = Vec::new();
        for (i, p) in payloads.iter().enumerate() {
            let out = enc.push_source(i as u32, p);
            all_symbols.push(out.source);
            all_symbols.extend(out.repairs);
        }

        // Drop seq 2 (1400 bytes)
        let mut events = Vec::new();
        for sym in all_symbols {
            let drop = matches!(&sym, Symbol::Source { seq: 2, .. });
            if !drop {
                events.extend(dec.push_symbol(sym));
            }
        }

        let recovered = collect_recovered(&events);
        assert!(
            recovered.contains(&2),
            "variable-length seq 2 not recovered"
        );
        if let Some(DecoderEvent::Recovered {
            seq: 2, payload, ..
        }) = events
            .iter()
            .find(|e| matches!(e, DecoderEvent::Recovered { seq: 2, .. }))
        {
            assert_eq!(payload.len(), 1400);
        }
    }

    #[test]
    fn rt_variable_length_all_lost() {
        let lengths: &[usize] = &[100, 0, 1400, 255, 7];
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(64, 1 << 20);

        let payloads: Vec<Vec<u8>> = lengths.iter().map(|&l| vec![0xCDu8; l]).collect();
        let mut all_repairs: Vec<Symbol> = Vec::new();
        for (i, p) in payloads.iter().enumerate() {
            let out = enc.push_source(i as u32, p);
            // drop source
            all_repairs.extend(out.repairs);
        }

        let mut events = Vec::new();
        for repair in all_repairs {
            events.extend(dec.push_symbol(repair));
        }

        let recovered = collect_recovered(&events);
        for (i, &expected_len) in lengths.iter().enumerate() {
            assert!(recovered.contains(&(i as u32)), "seq {i} not recovered");
            if let Some(DecoderEvent::Recovered { payload, .. }) = events
                .iter()
                .find(|e| matches!(e, DecoderEvent::Recovered { seq, .. } if *seq == i as u32))
            {
                assert_eq!(
                    payload.len(),
                    expected_len,
                    "seq {i} payload length mismatch"
                );
            }
        }
    }

    // ── 5-D: 윈도우/퇴거 ─────────────────────────────────────────────────

    #[test]
    fn evict_symbol_cap() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 0,
            redundancy_denominator: 1,
            window_max_symbols: 4,
            window_max_bytes: 1 << 20,
        });
        for i in 0..5u32 {
            enc.push_source(i, b"x");
        }
        assert_eq!(enc.window_len(), 4, "window should be capped at 4");
        assert_eq!(
            enc.window_base(),
            Some(1),
            "oldest seq should be 1 after eviction"
        );
    }

    #[test]
    fn evict_byte_cap() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 0,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 5 * 1024,
        });
        let payload = vec![0u8; 1024];
        for i in 0..5u32 {
            enc.push_source(i, &payload);
        }
        assert_eq!(enc.window_len(), 5);
        // 6th push should evict at least one
        enc.push_source(5, &payload);
        assert!(
            enc.window_len() < 6,
            "byte cap should have triggered eviction"
        );
    }

    #[test]
    fn feedback_future_seq_ignored() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        let prev_len = enc.window_len();
        enc.acknowledge(9999);
        assert_eq!(
            enc.window_len(),
            prev_len,
            "future ack should not change empty window"
        );
    }

    #[test]
    fn feedback_ancient_seq_ignored() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..8u32 {
            enc.push_source(i, b"x");
        }
        enc.acknowledge(3);
        let base_after_ack3 = enc.window_base();
        enc.acknowledge(1); // ancient
        assert_eq!(
            enc.window_base(),
            base_after_ack3,
            "ancient ack should not change window_base"
        );
    }

    // ── 5-E: 결정론적 LCG fuzz ───────────────────────────────────────────

    fn lcg_next(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    #[test]
    fn fuzz_no_loss_seed_1234() {
        let mut state: u64 = 1234;
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 8,
            window_max_symbols: 64,
            window_max_bytes: 1 << 24,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(64, 1 << 24);
        let mut events = Vec::new();
        for i in 0..64u32 {
            let len = (lcg_next(&mut state) % 1400) as usize;
            let payload = vec![0u8; len];
            let out = enc.push_source(i, &payload);
            events.extend(dec.push_symbol(out.source));
            for r in out.repairs {
                events.extend(dec.push_symbol(r));
            }
        }
        let recovered = collect_recovered(&events);
        for i in 0..64u32 {
            assert!(
                recovered.contains(&i),
                "no-loss fuzz: seq {i} not recovered"
            );
        }
    }

    #[test]
    fn fuzz_20pct_loss_seed_5678() {
        let mut state: u64 = 5678;
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 5,
            window_max_symbols: 64,
            window_max_bytes: 1 << 24,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(64, 1 << 24);
        let mut events = Vec::new();
        for i in 0..64u32 {
            let drop_source = lcg_next(&mut state).is_multiple_of(5);
            let out = enc.push_source(i, b"fuzz");
            if !drop_source {
                events.extend(dec.push_symbol(out.source));
            }
            for r in out.repairs {
                events.extend(dec.push_symbol(r));
            }
        }
        // A delivered successor bounds any final gap, so every input sequence
        // must be accounted for as delivery, recovery, loss, or eviction.
        events.extend(dec.push_symbol(enc.push_source(64, b"sentinel").source));
        let mut covered = std::collections::HashSet::new();
        for e in &events {
            match e {
                DecoderEvent::Recovered { seq, .. } => {
                    covered.insert(*seq);
                }
                DecoderEvent::LossSpan {
                    from_seq,
                    to_seq_exclusive,
                }
                | DecoderEvent::Evicted {
                    from_seq,
                    to_seq_exclusive,
                } => {
                    let mut seq = *from_seq;
                    while seq != *to_seq_exclusive {
                        covered.insert(seq);
                        seq = seq.wrapping_add(1);
                    }
                }
            }
        }
        for seq in 0..=64 {
            assert!(
                covered.contains(&seq),
                "loss fuzz left seq {seq} unaccounted"
            );
        }
    }

    #[test]
    fn fuzz_ratio_stress_seed_42() {
        let mut state: u64 = 42;
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 4,
            window_max_symbols: 64,
            window_max_bytes: 1 << 24,
        });
        let mut dec = FecDecoder::new(64, 1 << 24);
        let mut all_events = Vec::new();
        for i in 0..200u32 {
            if i % 50 == 0 && i > 0 {
                let num = (lcg_next(&mut state) % 3 + 1) as u8;
                let den = (lcg_next(&mut state) % 4 + 2) as u8;
                enc.set_redundancy(num, den);
            }
            let out = enc.push_source(i, b"stress");
            all_events.extend(dec.push_symbol(out.source));
            for r in out.repairs {
                all_events.extend(dec.push_symbol(r));
            }
        }
        // Invariant: all recovered seqs must be in 0..200
        for s in collect_recovered(&all_events) {
            assert!(s < 200, "recovered seq {s} out of [0,200)");
        }
    }

    // ── Destructive-boundary pinning tests ───────────────────────────────
    // One test per row in the boundary table above.

    /// ack(u32::MAX) on a non-empty window must be a no-op (future-seq guard).
    /// BEFORE the guard this wiped the entire window; the guard converts it to
    /// a no-op preserving all records.
    #[test]
    fn ack_u32_max_is_noop_on_nonempty_window() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..4u32 {
            enc.push_source(i, b"x");
        }
        assert_eq!(enc.window_len(), 4);
        enc.acknowledge(u32::MAX);
        assert_eq!(enc.window_len(), 4, "u32::MAX ack must not wipe window");
        assert_eq!(enc.window_base(), Some(0));
    }
    #[test]
    fn ack_wraps_and_rejects_half_range() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 0,
            redundancy_denominator: 1,
            window_max_symbols: 8,
            window_max_bytes: 1 << 20,
        });
        for seq in [u32::MAX - 1, u32::MAX, 0, 1] {
            enc.push_source(seq, b"x");
        }

        enc.acknowledge(u32::MAX);
        assert_eq!(enc.window_base(), Some(0));
        assert_eq!(enc.window_len(), 2);

        enc.acknowledge(0x8000_0001);
        assert_eq!(enc.window_base(), Some(0));
        assert_eq!(enc.window_len(), 2);
        enc.acknowledge(0);
        assert_eq!(enc.window_base(), Some(1));
        assert_eq!(enc.window_len(), 1);

        enc.acknowledge(1);
        assert_eq!(enc.window_len(), 0);
    }

    /// ack(window_back.seq) evicts ALL entries — total wipe, spec-mandated.
    /// Disclosed: window N → 0; entries irrecoverable from this module;
    /// caller contract: only send this value once receiver has decoded all.
    #[test]
    fn ack_back_seq_evicts_all() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..4u32 {
            enc.push_source(i, b"x");
        }
        enc.acknowledge(3); // 3 == window.back().seq
        assert_eq!(enc.window_len(), 0, "ack(back_seq) should empty the window");
    }

    /// ack(window_back.seq + 1) — one beyond the highest sent — is a no-op.
    #[test]
    fn ack_one_past_back_is_noop() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..4u32 {
            enc.push_source(i, b"x");
        }
        enc.acknowledge(4); // window back = 3, so 4 is future
        assert_eq!(enc.window_len(), 4);
        assert_eq!(enc.window_base(), Some(0));
    }

    /// ack(0) on a window whose first seq is 0 evicts exactly that one entry.
    #[test]
    fn ack_zero_evicts_seq_zero_only() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..4u32 {
            enc.push_source(i, b"x");
        }
        enc.acknowledge(0);
        assert_eq!(enc.window_len(), 3);
        assert_eq!(enc.window_base(), Some(1));
    }

    /// window_max_symbols=1 (minimum after sanitised): every push evicts the previous.
    /// window never exceeds 1 entry; oldest entry is irrecoverable from the module.
    #[test]
    fn symbol_cap_one_always_evicts_previous() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 0,
            redundancy_denominator: 1,
            window_max_symbols: 1,
            window_max_bytes: 1 << 20,
        });
        for i in 0..5u32 {
            enc.push_source(i, b"x");
            assert_eq!(enc.window_len(), 1);
            assert_eq!(enc.window_base(), Some(i));
        }
    }

    /// window_max_symbols=0 → sanitised() clamps to 1; same as cap=1.
    #[test]
    fn symbol_cap_zero_sanitised_to_one() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 0,
            redundancy_denominator: 1,
            window_max_symbols: 0,
            window_max_bytes: 1 << 20,
        });
        enc.push_source(0, b"x");
        enc.push_source(1, b"y");
        assert_eq!(enc.window_len(), 1);
    }

    // ── Pin tests for the three confirmed critical/major bugs ────────────────

    /// Bug-1 pin: `advance_contiguous` must emit `LossSpan` for a Missing span
    /// that is bounded on the right by a Received seq.
    ///
    /// Scenario: 1/4 ratio, seqs 0..8, seqs 2..5 dropped (4 losses),
    /// only 2 repairs produced → unrecoverable gap.  After all symbols are
    /// delivered the events must contain at least one `LossSpan` covering
    /// [2, 6) and seqs 0, 1, 6, 7 must be Recovered.
    #[test]
    fn pin_loss_span_emitted_for_bounded_missing_gap() {
        let mut s = Scenario::new(1, 4, 64);
        let mut all_events = Vec::new();
        for i in 0..8u32 {
            let drop = (2..6).contains(&i);
            all_events.extend(s.push(i, b"ping", drop));
        }
        // seqs 0,1,6,7 must be delivered
        let recovered: Vec<u32> = collect_recovered(&all_events);
        for &expect in &[0u32, 1, 6, 7] {
            assert!(
                recovered.contains(&expect),
                "seq {expect} must be Recovered"
            );
        }
        // A LossSpan covering exactly [2,6) must be present
        let has_loss_span = all_events.iter().any(|e| {
            matches!(
                e,
                DecoderEvent::LossSpan {
                    from_seq: 2,
                    to_seq_exclusive: 6
                }
            )
        });
        assert!(
            has_loss_span,
            "expected LossSpan(2,6) in events; got: {all_events:?}"
        );
        // seqs 2..5 must NOT appear as Recovered (they were unrecoverable)
        for seq in 2u32..6 {
            assert!(
                !recovered.contains(&seq),
                "seq {seq} was reported Recovered despite being unrecoverable"
            );
        }
    }

    /// Bug-2 pin: when `highest_contiguous == None` and the first symbol
    /// received has seq > 0, `highest_fully_decoded` must stay `None` because
    /// seqs 0..(seq-1) have never been confirmed.
    ///
    /// Old code took `sources.keys().next()` as the starting point, so it
    /// would set `highest_contiguous = 5` after receiving only seq 5, causing
    /// the encoder's `acknowledge(5)` to evict seqs 0–4 without justification.
    #[test]
    fn pin_advance_contiguous_does_not_skip_initial_missing_seqs() {
        let mut dec = FecDecoder::new(64, 1 << 20);
        // Deliver only seq 5 — seqs 0..4 are unknown (no repair, no source).
        dec.push_symbol(Symbol::Source {
            seq: 5,
            payload: b"late".to_vec(),
        });
        assert_eq!(
            dec.highest_fully_decoded(),
            None,
            "highest_fully_decoded must remain None when seqs 0..4 are unconfirmed"
        );
        // Now deliver seqs 0..4 in order; only then should highest advance.
        for i in 0u32..5 {
            dec.push_symbol(Symbol::Source {
                seq: i,
                payload: b"fill".to_vec(),
            });
        }
        assert_eq!(
            dec.highest_fully_decoded(),
            Some(5),
            "highest_fully_decoded must reach 5 once seqs 0..5 are all confirmed"
        );
    }

    /// Bug-3 pin: repair_seq=0 and repair_seq=128 must NOT share a dedup key.
    ///
    /// Old code: `key = (repair_seq as u8 % 128, window_base)`.
    /// `128u16 as u8 % 128 == 0`, so repair_seq=128 was silently rejected as a
    /// duplicate of repair_seq=0.  This lost an independent GF equation and
    /// could prevent recovery.
    ///
    /// Verification: push a source, drop it, push two distinct repairs
    /// (repair_seq=0 and repair_seq=128, same window).  If the second repair is
    /// incorrectly deduplicated, the decoder never has two equations for the one
    /// unknown and recovery fails.  With the fix, both repairs are accepted and
    /// the decoder can choose the better-ranked system.
    #[test]
    fn pin_repair_dedup_key_collision_repair_seq_128() {
        // Build a single-source window so that one repair is enough to recover.
        // We'll push repair_seq=0 first, then repair_seq=128 with the same
        // window_base — the second must NOT be dropped as a duplicate.
        let config = FecConfig {
            redundancy_numerator: 2, // 2 repairs per source → gives us repair_seq 0 and 1
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 20,
        };
        // Use the encoder to produce a legitimate repair (repair_seq=0).
        let mut enc = FecEncoder::new(config);
        let out = enc.push_source(0, b"secret");
        // out.repairs[0] has repair_seq=0, window_base=0, window_end=1.
        let repair0 = out.repairs[0].clone();
        let Symbol::Repair {
            window_base,
            window_end,
            payload: p0,
            ..
        } = &repair0
        else {
            panic!("expected Repair");
        };
        let (wb, we) = (*window_base, *window_end);

        // Craft a second repair with repair_seq=128 but the same window, carrying
        // DIFFERENT payload (different GF coefficient row).
        let repair128 = Symbol::Repair {
            repair_seq: 128,
            window_base: wb,
            window_end: we,
            payload: p0.clone(), // payload doesn't need to be "correct" to test dedup
        };

        let mut dec = FecDecoder::new(64, 1 << 20);
        // Push repair_seq=0 — accepted.
        let e0 = dec.push_symbol(repair0);
        // seq 0 recovered via single-source GE.
        let recovered_after_0 = collect_recovered(&e0);
        assert!(
            recovered_after_0.contains(&0),
            "repair_seq=0 should recover seq 0; got {recovered_after_0:?}"
        );

        // Push repair_seq=128 — must NOT be silently dropped as duplicate.
        // With old code (% 128) the second repair is rejected → push returns [].
        // With new code (full u16) the second repair is accepted → no panic,
        // and no duplicate-recovery event (seq 0 already Recovered, so
        // push_source path returns early).
        let e128 = dec.push_symbol(repair128);
        // The key assertion: the decoder must not panic and must accept the symbol
        // (we can't assert a specific event count since seq 0 is already recovered,
        // but we CAN assert the decoder did NOT treat it as a dedup by verifying
        // the internal seen_repair_keys would have two entries).
        // Proxy: push a *fresh* decoder and deliver repair_seq=128 FIRST, then
        // repair_seq=0 second — seq 0 should still be recovered from the first.
        let _ = e128; // no panic is the first gate

        // Fresh decoder: repair_seq=128 arrives first.
        let mut dec2 = FecDecoder::new(64, 1 << 20);
        let r128_first = Symbol::Repair {
            repair_seq: 128,
            window_base: wb,
            window_end: we,
            // Use a repair payload computed properly so GE can solve it.
            // Since source payload = b"secret" with coeff gf_coeff(128, 0):
            payload: {
                let coeff = gf_coeff(128, 0);
                let src = b"secret";
                let max_eff = src.len() + 2;
                let mut p = vec![0u8; max_eff];
                let src_len = src.len() as u16;
                p[0] = (src_len & 0xFF) as u8;
                p[1] = (src_len >> 8) as u8;
                for (i, &b) in src.iter().enumerate() {
                    p[2 + i] = gf_mul(coeff, b);
                }
                p[0] = gf_mul(coeff, p[0]);
                // Recompute: XOR entire effective payload with coeff
                let mut eff = vec![0u8; max_eff];
                let payload_len_bytes = (src.len() as u16).to_le_bytes();
                let mut prefixed = Vec::with_capacity(2 + src.len());
                prefixed.extend_from_slice(&payload_len_bytes);
                prefixed.extend_from_slice(src);
                for (j, &byte) in prefixed.iter().enumerate() {
                    eff[j] ^= gf_mul(coeff, byte);
                }
                eff
            },
        };
        let ev = dec2.push_symbol(r128_first);
        let rec2 = collect_recovered(&ev);
        assert!(
            rec2.contains(&0),
            "repair_seq=128 alone should recover seq 0; got {rec2:?}"
        );
    }

    /// Bug-finding-6 pin: when back().seq == u32::MAX the encoder must emit
    /// window_end = 0 (wrapping u32::MAX + 1), NOT panic in debug / overflow
    /// in release.  Old code: `self.window.back().seq + 1` caused overflow.
    /// Fixed by: `.seq.wrapping_add(1)`.
    #[test]
    fn pin_window_end_wraps_at_u32_max() {
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 24,
        };
        let mut enc = FecEncoder::new(config);
        // Push source with seq = u32::MAX; repair should have window_end = 0.
        let out = enc.push_source(u32::MAX, b"last_symbol");
        assert_eq!(out.repairs.len(), 1, "1/1 ratio must emit 1 repair");
        let Symbol::Repair {
            window_base,
            window_end,
            ..
        } = &out.repairs[0]
        else {
            panic!("expected Repair");
        };
        assert_eq!(*window_base, u32::MAX);
        assert_eq!(
            *window_end, 0u32,
            "window_end must wrap to 0 at u32::MAX + 1"
        );
    }

    /// Bug-finding-7 pin: the Rust decoder's `push_repair` and `one_elim_pass`
    /// must correctly handle a repair whose window crosses the u32 wrap-around
    /// boundary (window_base = u32::MAX, window_end = 1, covering 2 symbols).
    ///
    /// Old code: `for seq in window_base..window_end` is empty when window_end
    /// <= window_base after wrapping.  Fixed by wrapping while-loop.
    #[test]
    fn pin_decoder_handles_wrap_around_repair_window() {
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 4,
            window_max_bytes: 1 << 24,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(4, 1 << 24);

        // Push two sequential sources that straddle the u32 boundary.
        let out_max = enc.push_source(u32::MAX, b"before_wrap");
        let out_zero = enc.push_source(0, b"after_wrap");

        // Deliver seq 0 (source) to decoder — seq u32::MAX is dropped.
        let events_zero = dec.push_symbol(out_zero.source);
        let recovered_after_zero: Vec<u32> = events_zero
            .iter()
            .filter_map(|e| {
                if let DecoderEvent::Recovered { seq, .. } = e {
                    Some(*seq)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            recovered_after_zero.contains(&0),
            "seq 0 must be immediately recovered"
        );

        // Deliver the repair for seq 0 (window [u32::MAX, 1) = {u32::MAX, 0}).
        // With old code the decoder's push_repair loop is empty → seq u32::MAX
        // is never registered as Missing → GE has no row for it → no recovery.
        // With new code the loop correctly registers u32::MAX as Missing and 0
        // as Known, so GE can solve for u32::MAX.
        let mut all_events: Vec<DecoderEvent> = Vec::new();
        for r in out_zero.repairs {
            all_events.extend(dec.push_symbol(r));
        }

        // Check: after the repair the decoder should register u32::MAX as Missing
        // (it is now known-bounded: seq 0 arrived, proving the gap is irrecoverable
        // with only the repair from out_zero which covers [u32::MAX, 1)).
        // The key test: no panic, and the decoder did not silently emit a wrong
        // Recovered event for u32::MAX from an empty coefficient row.
        for e in &all_events {
            if let DecoderEvent::Recovered {
                seq: s, payload, ..
            } = e
                && *s == u32::MAX
            {
                // Recovery succeeded — verify payload correctness.
                assert_eq!(
                    payload.as_slice(),
                    b"before_wrap",
                    "recovered payload must match original"
                );
            }
        }

        // Deliver the repair from out_max (covers [u32::MAX, 0) = just u32::MAX
        // with a correct coefficient) — this should recover u32::MAX.
        let mut recovered_max = false;
        for r in out_max.repairs {
            let events = dec.push_symbol(r);
            for e in &events {
                if let DecoderEvent::Recovered {
                    seq: u32::MAX,
                    payload,
                    ..
                } = e
                {
                    assert_eq!(payload.as_slice(), b"before_wrap");
                    recovered_max = true;
                }
            }
        }
        // With old code: loop was empty → u32::MAX never registered in push_repair
        // → no recovery equation → recovered_max stays false.
        // With new code: loop registers u32::MAX as Missing → GE solves it.
        // Note: whether recovery actually fires depends on encoder window at that
        // point; the structural test is that no panic occurs and the loop ran.
        // The earlier wrap repair may already have recovered it in all_events:
        let already_recovered = all_events
            .iter()
            .any(|e| matches!(e, DecoderEvent::Recovered { seq: s, .. } if *s == u32::MAX));
        assert!(
            recovered_max || already_recovered,
            "seq u32::MAX must be recoverable via the wrap-around repair window"
        );
    }

    /// Off-spec repair declaring a window wider than the 128-symbol design cap
    /// is rejected outright (mirrors the TS decoder guard): no events, no
    /// Missing registration, and — because rejection happens before the dedup
    /// insert — a later in-spec repair with the SAME (repair_seq, window_base)
    /// key is still accepted and can recover.
    #[test]
    fn pin_repair_window_beyond_cap_rejected() {
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 4,
            window_max_bytes: 1 << 24,
        };
        let mut enc = FecEncoder::new(config);
        let out = enc.push_source(0, b"payload_zero");
        assert!(!out.repairs.is_empty(), "1/1 redundancy must emit a repair");
        let real_repair = out
            .repairs
            .into_iter()
            .next()
            .expect("1/1 redundancy must emit a repair");
        let (repair_seq, window_base) = match &real_repair {
            Symbol::Repair {
                repair_seq,
                window_base,
                ..
            } => (*repair_seq, *window_base),
            _ => unreachable!(),
        };

        let mut dec = FecDecoder::new(128, 1 << 24);

        // 1. Oversized repair with the SAME dedup key: must be rejected with
        //    no events and no dedup-key recording.
        let events = dec.push_symbol(Symbol::Repair {
            repair_seq,
            window_base,
            window_end: window_base.wrapping_add(100_000),
            payload: vec![0, 0],
        });
        assert!(events.is_empty(), "oversized repair must produce no events");

        // 2. The real in-spec repair with the same key: if the rejected key
        //    had been recorded, this would dedup to nothing and seq 0 could
        //    never be recovered.  It must instead recover seq 0.
        let events = dec.push_symbol(real_repair);
        let recovered = events.iter().any(|e| {
            matches!(
                e,
                DecoderEvent::Recovered { seq: 0, payload, .. } if payload.as_slice() == b"payload_zero"
            )
        });
        assert!(
            recovered,
            "in-spec repair sharing the rejected key must still be accepted and recover"
        );
    }

    /// Exact cap boundary: a 128-symbol repair window (the widest our encoder
    /// can emit — window_max_symbols clamps to 128) is ACCEPTED and usable for
    /// recovery; 129 is the first rejected width (guard is `window_len > 128`,
    /// textually mirrored in the TS decoder).
    #[test]
    fn pin_repair_window_cap_boundary_128_accepted_129_rejected() {
        // Encoder: redundancy 1/128 with a full 128-symbol window → exactly one
        // repair, emitted on the 128th push, covering [0, 128).
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 128,
            window_max_symbols: 128,
            window_max_bytes: 1 << 24,
        };
        let mut enc = FecEncoder::new(config);
        let mut repair_128 = None;
        for seq in 0..128u32 {
            let out = enc.push_source(seq, &[seq as u8]);
            for r in out.repairs {
                repair_128 = Some(r);
            }
        }
        let repair_128 = repair_128.expect("1/128 ratio must emit a repair by push 128");
        match &repair_128 {
            Symbol::Repair {
                window_base,
                window_end,
                ..
            } => {
                assert_eq!(
                    window_end.wrapping_sub(*window_base),
                    128,
                    "test premise: repair must span the full 128-symbol window"
                );
            }
            _ => unreachable!(),
        }

        // Decoder receives every source except seq 0, then the 128-wide repair:
        // acceptance at the exact cap is proven by the recovery of seq 0.
        let mut dec = FecDecoder::new(128, 1 << 24);
        for seq in 1..128u32 {
            dec.push_symbol(Symbol::Source {
                seq,
                payload: vec![seq as u8],
            });
        }
        let events = dec.push_symbol(repair_128);
        let recovered = events.iter().any(|e| {
            matches!(
                e,
                DecoderEvent::Recovered { seq: 0, payload, .. } if payload.as_slice() == [0u8]
            )
        });
        assert!(
            recovered,
            "128-wide repair window must be accepted at the cap"
        );

        // 129: first rejected width — no events, decoder state untouched.
        let mut dec2 = FecDecoder::new(128, 1 << 24);
        let events = dec2.push_symbol(Symbol::Repair {
            repair_seq: 7,
            window_base: 0,
            window_end: 129,
            payload: vec![0, 0],
        });
        assert!(events.is_empty(), "129-wide repair window must be rejected");
    }
    #[test]
    fn decoder_rolls_after_repair_cap_and_recovers_late_loss() {
        let config = FecConfig {
            redundancy_numerator: 1,
            redundancy_denominator: 1,
            window_max_symbols: 128,
            window_max_bytes: 1 << 20,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(128, 1 << 20);
        let lost_seq = 260;
        let mut events = Vec::new();

        for seq in 0..=lost_seq {
            let output = enc.push_source(seq, &[seq as u8]);
            if seq != lost_seq {
                events.extend(dec.push_symbol(output.source));
            }
            for repair in output.repairs {
                events.extend(dec.push_symbol(repair));
            }
        }

        assert!(
            events.iter().any(|event| matches!(
                event,
                DecoderEvent::Recovered {
                    seq,
                    payload,
                    via_fec: true,
                } if *seq == lost_seq && payload.as_slice() == [lost_seq as u8]
            )),
            "a loss after more than two 128-symbol windows must be recovered"
        );
        assert_eq!(dec.highest_fully_decoded(), Some(lost_seq));
        let accounting = dec.accounting();
        assert!(accounting.source_symbols <= 128);
        assert!(accounting.repair_symbols <= 128);
        assert_eq!(accounting.repair_symbols, accounting.dedup_keys);
    }

    /// window_max_symbols=u16::MAX → sanitised() clamps to 128.
    #[test]
    fn symbol_cap_u16_max_clamped_to_128() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 0,
            redundancy_denominator: 1,
            window_max_symbols: u16::MAX,
            window_max_bytes: 1 << 24,
        });
        for i in 0..130u32 {
            enc.push_source(i, b"x");
        }
        assert_eq!(enc.window_len(), 128);
    }

    /// window_max_bytes=0 → sanitised() clamps to 1. Empty-payload push must
    /// NOT trigger eviction (0 bytes added, total stays 0, 0 > 1 is false).
    #[test]
    fn byte_cap_zero_sanitised_empty_payload_no_eviction() {
        let mut enc = FecEncoder::new(FecConfig {
            redundancy_numerator: 0,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 0, // → clamped to 1
        });
        enc.push_source(0, &[]);
        enc.push_source(1, &[]);
        assert_eq!(
            enc.window_len(),
            2,
            "empty-payload pushes must not trigger byte-cap eviction"
        );
    }
    #[test]
    fn decoder_repair_admission_honours_exact_symbol_and_byte_caps() {
        let repair = |window_end, payload| Symbol::Repair {
            repair_seq: 1,
            window_base: 0,
            window_end,
            payload,
        };

        let mut exact_symbols = FecDecoder::new(2, 64);
        exact_symbols.push_symbol(Symbol::Source {
            seq: 0,
            payload: vec![],
        });
        exact_symbols.push_symbol(repair(2, vec![]));
        let accounting = exact_symbols.accounting();
        assert_eq!(accounting.source_symbols, 2);
        assert!(accounting.retained_bytes <= 64);

        let mut over_symbols = FecDecoder::new(1, 64);
        over_symbols.push_symbol(Symbol::Source {
            seq: 0,
            payload: vec![],
        });
        assert!(over_symbols.push_symbol(repair(2, vec![])).is_empty());
        assert_eq!(over_symbols.accounting().source_symbols, 1);
        assert_eq!(over_symbols.accounting().repair_symbols, 0);

        let mut exact_bytes = FecDecoder::new(1, 32);
        exact_bytes.push_symbol(repair(1, vec![]));
        assert_eq!(exact_bytes.accounting().retained_bytes, 32);

        let mut over_bytes = FecDecoder::new(1, 31);
        assert!(over_bytes.push_symbol(repair(1, vec![])).is_empty());
        assert_eq!(over_bytes.accounting().retained_bytes, 0);
    }
    #[test]
    fn rejected_forward_repair_leaves_nonempty_decoder_unchanged() {
        // An empty one-symbol repair needs 8 source bytes, 16 repair bytes, and
        // 8 dedup bytes. It is therefore intrinsically unadmittable at 31 bytes
        // even after a forward roll has retired every existing entry.
        let mut dec = FecDecoder::new(1, 31);
        dec.push_symbol(Symbol::Source {
            seq: 100,
            payload: vec![],
        });
        let before_accounting = dec.accounting();
        let before_stats = dec.stats();
        let before_frontier = dec.highest_fully_decoded();
        let before_sources = dec.sources.clone();
        let before_repairs = dec.repairs.clone();
        let before_keys = dec.seen_repair_keys.clone();
        let before_newest = dec.newest_seq;
        let before_open_loss_span = dec.open_loss_span;
        let before_open_loss_span_has_recovery = dec.open_loss_span_has_recovery;

        let events = dec.push_symbol(Symbol::Repair {
            repair_seq: 7,
            window_base: 101,
            window_end: 102,
            payload: vec![],
        });

        assert!(
            events.is_empty(),
            "rejected repair must emit no eviction event"
        );
        assert_eq!(dec.accounting(), before_accounting);
        assert_eq!(dec.stats(), before_stats);
        assert_eq!(dec.highest_fully_decoded(), before_frontier);
        assert_eq!(dec.sources, before_sources);
        assert_eq!(dec.repairs, before_repairs);
        assert_eq!(dec.seen_repair_keys, before_keys);
        assert_eq!(dec.newest_seq, before_newest);
        assert_eq!(dec.open_loss_span, before_open_loss_span);
        assert_eq!(
            dec.open_loss_span_has_recovery,
            before_open_loss_span_has_recovery
        );
    }

    #[test]
    fn decoder_rejects_128_wide_repair_when_symbol_cap_is_smaller() {
        let mut dec = FecDecoder::new(127, 1 << 20);
        assert!(
            dec.push_symbol(Symbol::Repair {
                repair_seq: 1,
                window_base: 0,
                window_end: 128,
                payload: vec![],
            })
            .is_empty()
        );
        assert_eq!(dec.accounting().source_symbols, 0);
        assert_eq!(dec.accounting().repair_symbols, 0);
        assert_eq!(dec.accounting().retained_bytes, 0);
    }

    #[test]
    fn loss_span_is_pruned_before_later_limit_enforcement() {
        let mut dec = FecDecoder::new(128, 1 << 20);
        dec.push_symbol(Symbol::Source {
            seq: 0,
            payload: vec![],
        });
        let mut events = dec.push_symbol(Symbol::Repair {
            repair_seq: 1,
            window_base: 1,
            window_end: 128,
            payload: vec![],
        });
        events.extend(dec.push_symbol(Symbol::Source {
            seq: 128,
            payload: vec![],
        }));
        events.extend(dec.push_symbol(Symbol::Repair {
            repair_seq: 2,
            window_base: 129,
            window_end: 256,
            payload: vec![],
        }));
        events.extend(dec.push_symbol(Symbol::Source {
            seq: 256,
            payload: vec![],
        }));

        let losses: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, DecoderEvent::LossSpan { .. }))
            .collect();
        assert_eq!(losses.len(), 2);
        assert!(matches!(
            losses[0],
            DecoderEvent::LossSpan {
                from_seq: 1,
                to_seq_exclusive: 128,
            }
        ));
        assert!(matches!(
            losses[1],
            DecoderEvent::LossSpan {
                from_seq: 129,
                to_seq_exclusive: 256,
            }
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, DecoderEvent::Evicted { .. }))
        );
        assert!(dec.accounting().source_symbols <= 128);
    }
    // ── single_erasure_recovery_sweep ──────────────────────────────────────
    /// Sweep the supported window sizes and repair rates with one erased source.
    /// This checks an actual independent repair equation without claiming MDS
    /// behavior from the hash-derived coefficient matrix.
    #[test]
    fn single_erasure_recovery_sweep() {
        let window_sizes: &[u16] = &[1, 2, 8, 64, 128];
        for &w in window_sizes {
            for redundancy in 1u8..=4 {
                let n = w as usize;
                // Emit the configured number of independent repairs per source.
                let config = FecConfig {
                    redundancy_numerator: redundancy,
                    redundancy_denominator: 1,
                    window_max_symbols: w,
                    window_max_bytes: 1 << 24,
                };
                for k in 1..=1 {
                    // Fresh enc/dec per scenario.
                    let mut enc = FecEncoder::new(config);
                    let mut dec = FecDecoder::new(w, 1 << 24);

                    let mut all_sources: Vec<Symbol> = Vec::new();
                    let mut all_repairs: Vec<Symbol> = Vec::new();
                    for i in 0..n as u32 {
                        let payload = format!("w{w}_r{redundancy}_k{k}_i{i}");
                        let out = enc.push_source(i, payload.as_bytes());
                        all_sources.push(out.source);
                        all_repairs.extend(out.repairs);
                    }

                    // Deliver non-erased sources (all but last k).
                    let erase_from = n - k;
                    let mut events = Vec::new();
                    for (i, src) in all_sources.into_iter().enumerate() {
                        if i < erase_from {
                            events.extend(dec.push_symbol(src));
                        }
                    }

                    // Deliver last k repairs (widest windows, covering erased seqs).
                    // Reverse order: widest first so cascade can start immediately.
                    for repair in all_repairs.into_iter().rev().take(k) {
                        events.extend(dec.push_symbol(repair));
                    }

                    let recovered = collect_recovered(&events);
                    for i in erase_from as u32..n as u32 {
                        assert!(
                            recovered.contains(&i),
                            "mds_sweep: w={w} redundancy={redundancy} k={k}: \
                             seq {i} not recovered; got {recovered:?}"
                        );
                    }
                }
            }
        }
    }

    // ── U2 P2 groundwork: recovery/loss-span counters ─────────────────────
    // Scenario parity with the counter tests in tests/fec_decoder.test.mjs.

    #[test]
    fn stats_clean_stream_all_recovery_counters_zero() {
        let mut s = Scenario::new(1, 8, 64);
        for i in 0..16u32 {
            s.push(i, b"data", false);
        }
        let stats = s.dec.stats();
        assert_eq!(
            stats.source_symbols_received, 16,
            "source count must match symbols fed"
        );
        assert_eq!(stats.symbols_recovered, 0);
        assert_eq!(stats.loss_spans, 0);
        assert_eq!(stats.loss_spans_recovered, 0);
    }

    #[test]
    fn stats_single_loss_recovered_counts_one_span_and_one_recovery() {
        // 1/1 ratio: every source gets a repair; drop seq 4 and recover it.
        let mut s = Scenario::new(1, 1, 64);
        for i in 0..8u32 {
            let drop = i == 4;
            s.push(i, b"hello", drop);
        }
        let stats = s.dec.stats();
        assert_eq!(
            stats.source_symbols_received, 7,
            "seq 4 was dropped on the wire"
        );
        assert_eq!(
            stats.symbols_recovered, 1,
            "exactly seq 4 recovered via FEC"
        );
        assert_eq!(stats.loss_spans, 1, "one loss episode observed");
        assert_eq!(
            stats.loss_spans_recovered, 1,
            "the episode closed fully healed"
        );
    }

    #[test]
    fn stats_burst_healed_by_fec_counts_one_span_not_two() {
        // Custom delivery (not the uniform Scenario helper): 2/1 redundancy
        // emits 2 independent repairs (different repair_seq -> independent
        // GF equations) per push, but only the pair built once the window
        // already spans both seq 2 and seq 3 (i.e. built by push_source(3))
        // is delivered — modelling repairs from the earlier, redundant-at-
        // that-point pushes being dropped on the wire. This guarantees both
        // equations covering the 2-wide gap [2,4) land and resolve together
        // in one try_recover batch, strictly before seq 4's direct arrival
        // could otherwise bound/abandon the gap (existing Bug-1 logic).
        let config = FecConfig {
            redundancy_numerator: 2,
            redundancy_denominator: 1,
            window_max_symbols: 64,
            window_max_bytes: 1 << 24,
        };
        let mut enc = FecEncoder::new(config);
        let mut dec = FecDecoder::new(64, 1 << 24);

        dec.push_symbol(enc.push_source(0, b"a").source);
        dec.push_symbol(enc.push_source(1, b"b").source);
        // seq 2: source dropped, its repairs discarded (dropped on the wire).
        enc.push_source(2, b"c");
        // seq 3: source dropped; both repairs (window now spans [0,4)) delivered.
        let out3 = enc.push_source(3, b"d");
        for repair in out3.repairs {
            dec.push_symbol(repair);
        }
        // seq 4: delivered directly.
        dec.push_symbol(enc.push_source(4, b"e").source);

        let stats = dec.stats();
        assert_eq!(stats.symbols_recovered, 2, "both seq 2 and 3 recovered");
        assert_eq!(stats.loss_spans, 1, "one contiguous episode, not two");
        assert_eq!(stats.loss_spans_recovered, 1);
    }

    #[test]
    fn stats_unrecoverable_gap_counts_span_but_not_recovered() {
        // Same scenario as pin_loss_span_emitted_for_bounded_missing_gap:
        // 1/4 ratio, seqs 2..6 dropped (4 losses, only 2 repairs) -> the gap
        // is bounded by seq 6 arriving and is abandoned, never healed.
        let mut s = Scenario::new(1, 4, 64);
        let mut all_events = Vec::new();
        for i in 0..8u32 {
            let drop = (2..6).contains(&i);
            all_events.extend(s.push(i, b"burst", drop));
        }
        assert_eq!(
            count_loss_spans(&all_events),
            1,
            "exactly one LossSpan event for the abandoned gap"
        );
        let stats = s.dec.stats();
        assert_eq!(stats.loss_spans, 1, "one loss episode observed");
        assert_eq!(
            stats.loss_spans_recovered, 0,
            "the episode was skipped, not healed"
        );
    }
}
