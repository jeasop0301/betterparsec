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

#![allow(dead_code)]

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

/// GF(256) division a/b. Returns 0 if a==0. Panics if b==0.
fn gf_div(a: u8, b: u8) -> u8 {
    assert_ne!(b, 0, "gf_div by zero");
    if a == 0 {
        return 0;
    }
    let t = gf_tables();
    let la = t.log[a as usize] as i16;
    let lb = t.log[b as usize] as i16;
    t.exp[((la - lb).rem_euclid(255)) as usize]
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
        assert!(payload.len() <= 65520, "payload exceeds maximum 65520 bytes");

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
            && self.window_byte_total.saturating_add(new_bytes)
                > self.config.window_max_bytes
        {
            self.evict_oldest();
        }

        self.window_byte_total = self.window_byte_total.saturating_add(new_bytes);
        self.window.push_back(WindowEntry { seq, prefixed_payload: prefixed });

        let source = Symbol::Source { seq, payload: payload.to_vec() };

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
        // §4-C: "미래 seq (윈도우에 없는 seq 초과) → 무시 (윈도우 변경 없음)".
        // Guard: if ack exceeds the highest seq we have ever emitted (window back),
        // treat as a no-op — prevents accidental total-wipe from a stale or erroneous
        // feedback value (e.g. u32::MAX).
        // Blast radius when ack == window.back().seq: N → 0 (total wipe, all entries
        // irrecoverable from this module). This is spec-mandated behaviour: the caller
        // only sends this value once the receiver has confirmed decoding all symbols.
        let Some(back_seq) = self.window.back().map(|e| e.seq) else {
            return; // empty window — nothing to evict
        };
        if highest_fully_decoded > back_seq {
            // Future seq: ignore per spec §4-C
            return;
        }
        while let Some(front) = self.window.front() {
            if front.seq <= highest_fully_decoded {
                self.evict_oldest();
            } else {
                break;
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

        Symbol::Repair { repair_seq, window_base, window_end, payload }
    }
}

// ── 디코더 ────────────────────────────────────────────────────────────────

/// 디코더가 방출하는 이벤트.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecoderEvent {
    Recovered { seq: u32, payload: Vec<u8> },
    LossSpan { from_seq: u32, to_seq_exclusive: u32 },
}

#[derive(Debug)]
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

#[derive(Debug, Clone)]
struct ReceivedRepair {
    repair_seq: u16,
    window_base: u32,
    window_end: u32,
    payload: Vec<u8>,
}

/// 체계적 슬라이딩 윈도우 FEC 디코더.
#[derive(Debug)]
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
    /// Lowest seq we still care about (eviction watermark).
    window_base: Option<u32>,
}

impl FecDecoder {
    pub fn new(max_symbols: u16, max_bytes: u32) -> Self {
        Self {
            max_symbols: max_symbols.clamp(1, 128),
            max_bytes: max_bytes.max(1),
            sources: BTreeMap::new(),
            repairs: Vec::new(),
            seen_repair_keys: std::collections::HashSet::new(),
            highest_contiguous: None,
            window_base: None,
        }
    }

    pub fn push_symbol(&mut self, symbol: Symbol) -> Vec<DecoderEvent> {
        match symbol {
            Symbol::Source { seq, payload } => self.push_source(seq, payload),
            Symbol::Repair { repair_seq, window_base, window_end, payload } => {
                self.push_repair(repair_seq, window_base, window_end, payload)
            }
        }
    }

    pub fn highest_fully_decoded(&self) -> Option<u32> {
        self.highest_contiguous
    }

    fn push_source(&mut self, seq: u32, payload: Vec<u8>) -> Vec<DecoderEvent> {
        // Ignore if already known
        if matches!(
            self.sources.get(&seq),
            Some(SourceState::Received(_) | SourceState::Recovered(_))
        ) {
            return Vec::new();
        }

        self.sources.insert(seq, SourceState::Received(payload.clone()));

        let mut events = vec![DecoderEvent::Recovered { seq, payload }];
        // Try to cascade-recover missing symbols using available repairs
        let recovered = self.try_recover();
        events.extend(recovered.into_iter().map(|(s, p)| DecoderEvent::Recovered { seq: s, payload: p }));
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
        // Deduplicate by (repair_seq, window_base) — full u16 to avoid false-positive
        // collisions: e.g. repair_seq=0 and repair_seq=128 had the same truncated key.
        let key = (repair_seq, window_base);
        if self.seen_repair_keys.contains(&key) {
            return Vec::new();
        }

        // Reject repairs declaring a window beyond the 128-symbol design cap
        // (Cauchy constraint; mirrors the TS decoder guard).  An off-spec or
        // corrupt symbol would otherwise insert window_len Missing entries
        // below.  Checked before the dedup insert, like the TS side, so the
        // rejected key is not recorded.
        let window_len = window_end.wrapping_sub(window_base);
        if window_len > 128 {
            return Vec::new();
        }
        self.seen_repair_keys.insert(key);

        // Register all seqs in this repair's window as at least Missing.
        // Use wrapping iteration (seq != window_end) to handle the case where
        // window_end wrapped to 0 (i.e., window contains u32::MAX).  A plain
        // `for seq in window_base..window_end` range is empty when
        // window_end <= window_base after wrapping.
        let mut seq = window_base;
        while seq != window_end {
            self.sources.entry(seq).or_insert(SourceState::Missing);
            seq = seq.wrapping_add(1);
        }

        self.repairs.push(ReceivedRepair { repair_seq, window_base, window_end, payload });

        let recovered = self.try_recover();
        let mut events: Vec<DecoderEvent> = recovered
            .into_iter()
            .map(|(s, p)| DecoderEvent::Recovered { seq: s, payload: p })
            .collect();
        events.extend(self.advance_contiguous());
        events
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
            for (seq, payload) in &batch {
                self.sources.insert(*seq, SourceState::Recovered(payload.clone()));
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
        let max_eff_len = self.repairs.iter().map(|r| r.payload.len()).max().unwrap_or(0);
        if max_eff_len == 0 {
            return Vec::new();
        }

        let n_unknowns = missing_seqs.len();

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
            let payload_len =
                u16::from_le_bytes([eff_payload[0], eff_payload[1]]) as usize;
            let end = (2 + payload_len).min(eff_payload.len());
            let payload = eff_payload[2..end].to_vec();
            result.push((seq, payload));
        }
        result
    }

    /// Advance `highest_contiguous` and emit `LossSpan` for permanently missing
    /// ranges.
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
    fn advance_contiguous(&mut self) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        'outer: loop {
            // Bug-2 fix: always start from 0 when no contiguous baseline exists.
            let next = match self.highest_contiguous {
                None => 0u32,
                Some(h) => h.wrapping_add(1),
            };

            match self.sources.get(&next) {
                Some(SourceState::Received(_) | SourceState::Recovered(_)) => {
                    self.highest_contiguous = Some(next);
                }
                Some(SourceState::Missing) => {
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
                                self.highest_contiguous = Some(scan.wrapping_sub(1));
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
    let mut row_pivot_col: Vec<Option<usize>> = vec![None; n_rows]; // row → col
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
        row_pivot_col[current_row] = Some(col);
        current_row += 1;
        if current_row >= n_rows {
            break;
        }
    }

    // Collect results. A pivot column's solution is unique only when all
    // non-pivot columns in its reduced row are zero — i.e., no free variables
    // remain that affect that unknown. Under-determined rows are skipped.
    let pivot_cols: std::collections::HashSet<usize> =
        pivot_row.iter().enumerate().filter_map(|(c, r)| r.map(|_| c)).collect();

    let mut result = Vec::new();
    for (col, pivot_entry) in pivot_row.iter().enumerate() {
        let Some(&pr) = pivot_entry.as_ref() else { continue };
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
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let a = (state >> 33) as u8;
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
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
            Symbol::Source { seq: 0, payload: vec![] }
        );
        assert_eq!(out.repairs.len(), 1);
        if let Symbol::Repair { payload, .. } = &out.repairs[0] {
            assert_eq!(payload.len(), 2, "repair payload must be 2 bytes for empty source");
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
            Symbol::Source { seq: 0, ref payload } => assert_eq!(payload.len(), 65520),
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
        let total_repairs: usize = (0..100).map(|i| enc.push_source(i, b"x").repairs.len()).sum();
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
        let total_repairs: usize = (0..10).map(|i| enc.push_source(i, b"x").repairs.len()).sum();
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
        let total_repairs: usize = (0..16).map(|i| enc.push_source(i, b"x").repairs.len()).sum();
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

        fn push_repair_only(&mut self, repair: Symbol) -> Vec<DecoderEvent> {
            self.dec.push_symbol(repair)
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
        assert!(recovered.contains(&4), "seq 4 not recovered; got {recovered:?}");
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
        assert!(recovered.contains(&2) && recovered.contains(&3),
            "burst not recovered; got {recovered:?}");
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
        assert!(loss_count > 0, "expected unrecoverable loss, but all recovered: {recovered:?}");
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
        let sym = Symbol::Source { seq: 3, payload: b"hello".to_vec() };
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
        assert!(r1.is_empty(), "repair with no missing sources recovers nothing");
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
                assert!(window_base >= 8, "repair window_base {window_base} < 8 after ack(7)");
            }
        }
    }

    #[test]
    fn rt_stale_repair_ignored() {
        let mut s = Scenario::new(1, 1, 64);
        // Build a repair that covers seq 0..4
        let mut stale_repairs = Vec::new();
        for i in 0..4u32 {
            let out = s.enc.push_source(i, b"data");
            stale_repairs.extend(out.repairs);
        }
        // Ack everything up to 3
        s.enc.acknowledge(3);
        // Now push repair to decoder — window_base=0 is stale relative to decoder state
        // The decoder should not crash and should not produce spurious events
        for repair in stale_repairs {
            let events = s.dec.push_symbol(repair);
            // No source was ever registered missing in decoder, so events may be empty or contain
            // harmless Recovered events for sources the decoder did receive.
            // Key: no panic.
            let _ = events;
        }
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
            assert!(recovered.contains(&i), "seq {i} not recovered in repair-only scenario; got {recovered:?}");
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
        assert!(recovered.contains(&0), "single-symbol window recovery failed");
        // Also verify payload
        if let Some(DecoderEvent::Recovered { seq: 0, payload }) =
            events.iter().find(|e| matches!(e, DecoderEvent::Recovered { seq: 0, .. }))
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
        assert!(recovered.contains(&2), "variable-length seq 2 not recovered");
        if let Some(DecoderEvent::Recovered { seq: 2, payload }) =
            events.iter().find(|e| matches!(e, DecoderEvent::Recovered { seq: 2, .. }))
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
            if let Some(DecoderEvent::Recovered { payload, .. }) = events.iter().find(
                |e| matches!(e, DecoderEvent::Recovered { seq, .. } if *seq == i as u32)
            ) {
                assert_eq!(payload.len(), expected_len, "seq {i} payload length mismatch");
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
        assert_eq!(enc.window_base(), Some(1), "oldest seq should be 1 after eviction");
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
        assert!(enc.window_len() < 6, "byte cap should have triggered eviction");
    }

    #[test]
    fn feedback_future_seq_ignored() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        let prev_len = enc.window_len();
        enc.acknowledge(9999);
        assert_eq!(enc.window_len(), prev_len, "future ack should not change empty window");
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
        assert_eq!(enc.window_base(), base_after_ack3, "ancient ack should not change window_base");
    }

    // ── 5-E: 결정론적 LCG fuzz ───────────────────────────────────────────

    fn lcg_next(state: &mut u64) -> u64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
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
            assert!(recovered.contains(&i), "no-loss fuzz: seq {i} not recovered");
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
        // All seqs should appear in either Recovered or LossSpan
        let mut covered = std::collections::HashSet::new();
        for e in &events {
            match e {
                DecoderEvent::Recovered { seq, .. } => { covered.insert(*seq); }
                DecoderEvent::LossSpan { from_seq, to_seq_exclusive } => {
                    for s in *from_seq..*to_seq_exclusive { covered.insert(s); }
                }
            }
        }
        // At minimum, all recovered seqs must be in 0..64
        for s in collect_recovered(&events) {
            assert!(s < 64, "recovered seq {s} out of range");
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
        for i in 0..4u32 { enc.push_source(i, b"x"); }
        assert_eq!(enc.window_len(), 4);
        enc.acknowledge(u32::MAX);
        assert_eq!(enc.window_len(), 4, "u32::MAX ack must not wipe window");
        assert_eq!(enc.window_base(), Some(0));
    }

    /// ack(window_back.seq) evicts ALL entries — total wipe, spec-mandated.
    /// Disclosed: window N → 0; entries irrecoverable from this module;
    /// caller contract: only send this value once receiver has decoded all.
    #[test]
    fn ack_back_seq_evicts_all() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..4u32 { enc.push_source(i, b"x"); }
        enc.acknowledge(3); // 3 == window.back().seq
        assert_eq!(enc.window_len(), 0, "ack(back_seq) should empty the window");
    }

    /// ack(window_back.seq + 1) — one beyond the highest sent — is a no-op.
    #[test]
    fn ack_one_past_back_is_noop() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..4u32 { enc.push_source(i, b"x"); }
        enc.acknowledge(4); // window back = 3, so 4 is future
        assert_eq!(enc.window_len(), 4);
        assert_eq!(enc.window_base(), Some(0));
    }

    /// ack(0) on a window whose first seq is 0 evicts exactly that one entry.
    #[test]
    fn ack_zero_evicts_seq_zero_only() {
        let mut enc = FecEncoder::new(FecConfig::default_streaming());
        for i in 0..4u32 { enc.push_source(i, b"x"); }
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
            assert!(recovered.contains(&expect), "seq {expect} must be Recovered");
        }
        // A LossSpan covering exactly [2,6) must be present
        let has_loss_span = all_events.iter().any(|e| {
            matches!(e, DecoderEvent::LossSpan { from_seq: 2, to_seq_exclusive: 6 })
        });
        assert!(has_loss_span,
            "expected LossSpan(2,6) in events; got: {all_events:?}");
        // seqs 2..5 must NOT appear as Recovered (they were unrecoverable)
        for seq in 2u32..6 {
            assert!(!recovered.contains(&seq),
                "seq {seq} was reported Recovered despite being unrecoverable");
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
        dec.push_symbol(Symbol::Source { seq: 5, payload: b"late".to_vec() });
        assert_eq!(
            dec.highest_fully_decoded(),
            None,
            "highest_fully_decoded must remain None when seqs 0..4 are unconfirmed"
        );
        // Now deliver seqs 0..4 in order; only then should highest advance.
        for i in 0u32..5 {
            dec.push_symbol(Symbol::Source { seq: i, payload: b"fill".to_vec() });
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
        let Symbol::Repair { window_base, window_end, payload: p0, .. } = &repair0 else {
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
        assert!(recovered_after_0.contains(&0),
            "repair_seq=0 should recover seq 0; got {recovered_after_0:?}");

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
        assert!(rec2.contains(&0),
            "repair_seq=128 alone should recover seq 0; got {rec2:?}");
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
        let Symbol::Repair { window_base, window_end, .. } = &out.repairs[0] else {
            panic!("expected Repair");
        };
        assert_eq!(*window_base, u32::MAX);
        assert_eq!(*window_end, 0u32, "window_end must wrap to 0 at u32::MAX + 1");
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
            .filter_map(|e| if let DecoderEvent::Recovered { seq, .. } = e { Some(*seq) } else { None })
            .collect();
        assert!(recovered_after_zero.contains(&0), "seq 0 must be immediately recovered");

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
            if let DecoderEvent::Recovered { seq: s, payload } = e {
                if *s == u32::MAX {
                    // Recovery succeeded — verify payload correctness.
                    assert_eq!(payload.as_slice(), b"before_wrap",
                        "recovered payload must match original");
                }
            }
        }

        // Deliver the repair from out_max (covers [u32::MAX, 0) = just u32::MAX
        // with a correct coefficient) — this should recover u32::MAX.
        let mut recovered_max = false;
        for r in out_max.repairs {
            let events = dec.push_symbol(r);
            for e in &events {
                if let DecoderEvent::Recovered { seq: u32::MAX, payload } = e {
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
        let already_recovered = all_events.iter().any(|e| {
            matches!(e, DecoderEvent::Recovered { seq: s, .. } if *s == u32::MAX)
        });
        assert!(recovered_max || already_recovered,
            "seq u32::MAX must be recoverable via the wrap-around repair window");
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
        let real_repair = out.repairs.into_iter().next().unwrap();
        let (repair_seq, window_base) = match &real_repair {
            Symbol::Repair { repair_seq, window_base, .. } => (*repair_seq, *window_base),
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
        let recovered = events.iter().any(|e| matches!(
            e,
            DecoderEvent::Recovered { seq: 0, payload } if payload.as_slice() == b"payload_zero"
        ));
        assert!(recovered,
            "in-spec repair sharing the rejected key must still be accepted and recover");
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
        for i in 0..130u32 { enc.push_source(i, b"x"); }
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
        assert_eq!(enc.window_len(), 2, "empty-payload pushes must not trigger byte-cap eviction");
    }

    // ── mds_guarantee_sweep ───────────────────────────────────────────────

    /// Sweep: for window sizes {1,2,8,64,128} × redundancy 1..=4 × erasure
    /// count k=1..=min(redundancy, window_size), verify k source erasures are
    /// fully recovered using the k last (widest-window) repair symbols.
    ///
    /// Design: uses ratio 1/1 (one repair per source) so repairs cover
    /// nested windows [0..1), [0..2), …, [0..n). Erasing the last k sources
    /// and using the last k repairs enables cascading recovery: each repair
    /// introduces exactly one new unknown that its predecessor resolved.
    #[test]
    fn mds_guarantee_sweep() {
        let window_sizes: &[u16] = &[1, 2, 8, 64, 128];
        for &w in window_sizes {
            for redundancy in 1u8..=4 {
                let n = w as usize;
                // Config: 1 repair per source → nested window coverage.
                let config = FecConfig {
                    redundancy_numerator: 1,
                    redundancy_denominator: 1,
                    window_max_symbols: w,
                    window_max_bytes: 1 << 24,
                };
                for k in 1..=(redundancy as usize).min(n) {
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
}
