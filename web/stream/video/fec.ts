// Tetrys-style systematic sliding-window FEC codec — TypeScript port of streamer/src/fec.rs
// GF(256) polynomial x^8+x^4+x^3+x^2+1 (0x11D), generator g = 0x02.

// ── GF(256) tables ────────────────────────────────────────────────────────
const GF_EXP = new Uint8Array(512);
const GF_LOG = new Uint8Array(256);

(function buildTables() {
    let x = 1;
    for (let i = 0; i < 255; i++) {
        GF_EXP[i] = x;
        GF_EXP[i + 255] = x;
        GF_LOG[x] = i;
        const carry = x & 0x80;
        x = (x << 1) & 0xFF;
        if (carry !== 0) x ^= 0x1D;
    }
    GF_EXP[255] = GF_EXP[0]; // g^255 = 1
    GF_EXP[510] = GF_EXP[255];
})();

function gfMul(a: number, b: number): number {
    if (a === 0 || b === 0) return 0;
    return GF_EXP[GF_LOG[a] + GF_LOG[b]];
}

function gfInv(a: number): number {
    // precondition: a !== 0
    return GF_EXP[255 - GF_LOG[a]];
}

function gfDiv(a: number, b: number): number {
    // precondition: b !== 0
    if (a === 0) return 0;
    const la = GF_LOG[a];
    const lb = GF_LOG[b];
    return GF_EXP[((la - lb) % 255 + 255) % 255];
}

/** Deterministic GF(256) coefficient for (repairSeq, srcSeq). Always non-zero. */
function gfCoeff(repairSeq: number, srcSeq: number): number {
    const part1 = (Math.imul(repairSeq & 0xFFFF, 0x9E3779B9)) >>> 0;
    const part2 = (Math.imul(srcSeq >>> 0, 0x6B436201)) >>> 0;
    const h = (part1 + part2) >>> 0;
    const c = ((h >>> 24) ^ (h >>> 16) ^ (h >>> 8) ^ h) & 0xFF;
    return c === 0 ? 0x01 : c;
}

// ── FecConfig ─────────────────────────────────────────────────────────────

export interface FecConfig {
    redundancyNumerator: number;   // u8
    redundancyDenominator: number; // u8, 0 → treated as 1
    windowMaxSymbols: number;      // u16, clamped 1..=128
    windowMaxBytes: number;        // u32
}

export function defaultStreamingConfig(): FecConfig {
    return {
        redundancyNumerator: 1,
        redundancyDenominator: 8,
        windowMaxSymbols: 64,
        windowMaxBytes: 524_288,
    };
}

function sanitiseConfig(cfg: FecConfig): FecConfig {
    return {
        redundancyNumerator: cfg.redundancyNumerator,
        redundancyDenominator: Math.max(cfg.redundancyDenominator, 1),
        windowMaxSymbols: Math.min(Math.max(cfg.windowMaxSymbols, 1), 128),
        windowMaxBytes: Math.max(cfg.windowMaxBytes, 1),
    };
}

// ── Symbol types ──────────────────────────────────────────────────────────

export type SourceSymbol = {
    kind: 'source';
    seq: number;       // u32
    payload: Uint8Array;
};

export type RepairSymbol = {
    kind: 'repair';
    repairSeq: number;    // u16
    windowBase: number;   // u32
    windowEnd: number;    // u32
    payload: Uint8Array;
};

export type FecSymbol = SourceSymbol | RepairSymbol;

// ── DecoderEvent types ────────────────────────────────────────────────────

export type RecoveredEvent = {
    kind: 'recovered';
    seq: number;       // u32
    payload: Uint8Array;
};

export type LossSpanEvent = {
    kind: 'lossSpan';
    fromSeq: number;          // u32
    toSeqExclusive: number;   // u32
};

export type DecoderEvent = RecoveredEvent | LossSpanEvent;

// ── FecEncoder ────────────────────────────────────────────────────────────

interface WindowEntry {
    seq: number;                // u32
    prefixedPayload: Uint8Array; // 2-byte LE length prefix + original payload
}

export interface EncoderOutput {
    source: FecSymbol;
    repairs: FecSymbol[];
}

export class FecEncoder {
    private config: FecConfig;
    private window: WindowEntry[] = [];
    private windowByteTotal = 0; // tracks original payload bytes only
    private nextRepairSeq = 0;   // u16, wraps at 65536
    private ratioAcc = 0;        // u16 accumulator

    constructor(config: FecConfig) {
        this.config = sanitiseConfig(config);
    }

    setRedundancy(numerator: number, denominator: number): void {
        this.config.redundancyNumerator = numerator;
        this.config.redundancyDenominator = Math.max(denominator, 1);
    }

    pushSource(seq: number, payload: Uint8Array): EncoderOutput {
        const payloadLen = payload.length;
        // Build length-prefixed entry: [len_lo, len_hi, ...payload]
        const prefixed = new Uint8Array(2 + payloadLen);
        prefixed[0] = payloadLen & 0xFF;
        prefixed[1] = (payloadLen >> 8) & 0xFF;
        prefixed.set(payload, 2);

        // Evict to enforce caps before adding
        while (this.window.length >= this.config.windowMaxSymbols) {
            this.evictOldest();
        }
        const newBytes = payloadLen;
        while (
            this.window.length > 0 &&
            (this.windowByteTotal + newBytes) > this.config.windowMaxBytes
        ) {
            this.evictOldest();
        }

        this.windowByteTotal += newBytes;
        this.window.push({ seq: seq >>> 0, prefixedPayload: prefixed });

        const source: FecSymbol = { kind: 'source', seq: seq >>> 0, payload };

        // Ratio counter: accumulate numerator, emit repair when >= denominator
        const repairs: FecSymbol[] = [];
        if (this.config.redundancyNumerator > 0) {
            this.ratioAcc += this.config.redundancyNumerator;
            while (this.ratioAcc >= this.config.redundancyDenominator) {
                this.ratioAcc -= this.config.redundancyDenominator;
                repairs.push(this.buildRepair());
            }
        }

        return { source, repairs };
    }

    acknowledge(highestFullyDecoded: number): void {
        const hfd = highestFullyDecoded >>> 0;
        if (this.window.length === 0) return;
        const backSeq = this.window[this.window.length - 1].seq;
        // Future-seq guard: ignore acks beyond the highest seq we have emitted
        if (hfd > backSeq) return;
        while (this.window.length > 0 && this.window[0].seq <= hfd) {
            this.evictOldest();
        }
    }

    windowLen(): number { return this.window.length; }

    windowBase(): number | null {
        return this.window.length > 0 ? this.window[0].seq : null;
    }

    private evictOldest(): void {
        const entry = this.window.shift();
        if (entry !== undefined) {
            // prefixedPayload = 2 len bytes + payload; track only payload bytes
            const payloadBytes = Math.max(entry.prefixedPayload.length - 2, 0);
            this.windowByteTotal = Math.max(this.windowByteTotal - payloadBytes, 0);
        }
    }

    private buildRepair(): FecSymbol {
        const repairSeq = this.nextRepairSeq;
        this.nextRepairSeq = (this.nextRepairSeq + 1) & 0xFFFF;

        if (this.window.length === 0) {
            return {
                kind: 'repair', repairSeq, windowBase: 0, windowEnd: 0,
                payload: new Uint8Array(0),
            };
        }

        const windowBase = this.window[0].seq;
        const windowEnd = (this.window[this.window.length - 1].seq + 1) >>> 0;

        // max effective length = max(prefixedPayload.length) over window
        let maxEffLen = 0;
        for (const e of this.window) {
            if (e.prefixedPayload.length > maxEffLen) maxEffLen = e.prefixedPayload.length;
        }

        // XOR combine: repair[j] = XOR_i( gfMul(gfCoeff(repairSeq, src_i.seq), eff[j]) )
        const payload = new Uint8Array(maxEffLen);
        for (const entry of this.window) {
            const coeff = gfCoeff(repairSeq, entry.seq);
            const pp = entry.prefixedPayload;
            for (let j = 0; j < pp.length; j++) {
                payload[j] ^= gfMul(coeff, pp[j]);
            }
        }

        return { kind: 'repair', repairSeq, windowBase, windowEnd, payload };
    }
}

// ── FecDecoder ────────────────────────────────────────────────────────────

type SourceState =
    | { kind: 'received';  payload: Uint8Array }
    | { kind: 'recovered'; payload: Uint8Array }
    | { kind: 'missing' };

interface StoredRepair {
    repairSeq: number;   // u16
    windowBase: number;  // u32
    windowEnd: number;   // u32
    payload: Uint8Array;
}

export class FecDecoder {
    private readonly maxSymbols: number; // stored for reference; not enforced as eviction
    private readonly maxBytes: number;
    private sources = new Map<number, SourceState>();
    private repairs: StoredRepair[] = [];
    // Maps `${repairSeq},${windowBase}` → windowBase (number) for lazy pruning.
    // Value stored as a number so we can evict stale entries without a reverse map.
    private seenRepairKeys = new Map<string, number>();
    private highestContiguous: number | null = null; // null = None

    constructor(maxSymbols: number, maxBytes: number) {
        this.maxSymbols = Math.min(Math.max(maxSymbols, 1), 128);
        this.maxBytes = Math.max(maxBytes, 1);
    }

    pushSymbol(symbol: FecSymbol): DecoderEvent[] {
        if (symbol.kind === 'source') {
            return this.pushSource(symbol.seq, symbol.payload);
        } else {
            return this.pushRepair(
                symbol.repairSeq, symbol.windowBase, symbol.windowEnd, symbol.payload,
            );
        }
    }

    highestFullyDecoded(): number | null {
        return this.highestContiguous;
    }

    private pushSource(seq: number, payload: Uint8Array): DecoderEvent[] {
        const s = seq >>> 0;
        const existing = this.sources.get(s);
        if (existing !== undefined && existing.kind !== 'missing') {
            return []; // already Received or Recovered
        }
        this.sources.set(s, { kind: 'received', payload });
        const events: DecoderEvent[] = [{ kind: 'recovered', seq: s, payload }];
        const recovered = this.tryRecover();
        for (const [rs, rp] of recovered) {
            events.push({ kind: 'recovered', seq: rs, payload: rp });
        }
        events.push(...this.advanceContiguous());
        return events;
    }

    private pushRepair(
        repairSeq: number, windowBase: number, windowEnd: number, payload: Uint8Array,
    ): DecoderEvent[] {
        // Deduplicate by (repairSeq, windowBase) — full u16 repairSeq (Bug-3 fix)
        const key = `${repairSeq & 0xFFFF},${windowBase >>> 0}`;
        if (this.seenRepairKeys.has(key)) return [];
        const wb = windowBase >>> 0;
        const we = windowEnd >>> 0;

        // Guard: reject repair symbols with windows larger than the design cap of
        // 128 symbols (design §4).  A window of e.g. 100 000 would insert 100 000
        // Missing entries and stall the event loop — this is an off-spec or
        // adversarially crafted symbol.
        const windowLen = (we - wb + 0x100000000) >>> 0;
        if (windowLen > 128) return [];

        // Commit dedup key now that the window has been validated.
        // Stored value is windowBase so the pruning pass can identify stale entries.
        this.seenRepairKeys.set(key, wb);

        // Register all seqs in this repair's window as at least Missing
        for (let seq = wb; seq !== we; seq = (seq + 1) >>> 0) {
            if (!this.sources.has(seq)) {
                this.sources.set(seq, { kind: 'missing' });
            }
        }

        this.repairs.push({ repairSeq: repairSeq & 0xFFFF, windowBase: wb, windowEnd: we, payload });

        const recovered = this.tryRecover();
        const events: DecoderEvent[] = recovered.map(
            ([s, p]) => ({ kind: 'recovered' as const, seq: s, payload: p }),
        );
        events.push(...this.advanceContiguous());

        // Lazily prune seenRepairKeys when it grows large.  At 60 fps / 1/8 ratio
        // the map grows at ~22.5 entries/sec; pruning at >256 keeps the long-term
        // footprint bounded without paying O(n) on every symbol.
        if (this.seenRepairKeys.size > 256 && this.highestContiguous !== null) {
            // Entries with windowBase more than 128 symbols behind highestContiguous
            // can never contribute to recovery — the whole window has been resolved.
            // Integer subtraction without u32 wrap guard; safe for sessions shorter
            // than ~276 days at 180 sym/sec (u32::MAX).
            const pruneThreshold = this.highestContiguous - 128;
            for (const [k, base] of this.seenRepairKeys) {
                if (base <= pruneThreshold) {
                    this.seenRepairKeys.delete(k);
                }
            }
        }

        return events;
    }

    /** Iteratively recover Missing symbols via Gaussian elimination. */
    private tryRecover(): Array<[number, Uint8Array]> {
        const allRecovered: Array<[number, Uint8Array]> = [];
        for (;;) {
            const batch = this.oneElimPass();
            if (batch.length === 0) break;
            for (const [seq, payload] of batch) {
                this.sources.set(seq, { kind: 'recovered', payload });
            }
            allRecovered.push(...batch);
        }
        return allRecovered;
    }

    /** Single Gaussian-elimination pass. Returns (seq, payload) for new recoveries. */
    private oneElimPass(): Array<[number, Uint8Array]> {
        if (this.repairs.length === 0) return [];

        // Collect all Missing seqs sorted ascending
        const missingSeqs: number[] = [];
        for (const [seq, state] of this.sources) {
            if (state.kind === 'missing') missingSeqs.push(seq);
        }
        if (missingSeqs.length === 0) return [];
        missingSeqs.sort((a, b) => (a >>> 0) < (b >>> 0) ? -1 : (a >>> 0) > (b >>> 0) ? 1 : 0);

        // Max effective payload length across all repairs
        let maxEffLen = 0;
        for (const r of this.repairs) {
            if (r.payload.length > maxEffLen) maxEffLen = r.payload.length;
        }
        if (maxEffLen === 0) return [];

        const nUnknowns = missingSeqs.length;
        const coeffMatrix: Uint8Array[] = [];
        const rhsMatrix: Uint8Array[] = [];

        for (const repair of this.repairs) {
            const rowCoeffs = new Uint8Array(nUnknowns);
            const rowRhs = new Uint8Array(maxEffLen);
            // Initialize RHS from repair payload (zero-extended to maxEffLen)
            rowRhs.set(repair.payload);

            const wb = repair.windowBase;
            const we = repair.windowEnd;
            for (let seq = wb; seq !== we; seq = (seq + 1) >>> 0) {
                const coeff = gfCoeff(repair.repairSeq, seq);
                const state = this.sources.get(seq);
                if (state === undefined) continue; // outside decoder scope
                if (state.kind !== 'missing') {
                    // Known: subtract its contribution from RHS
                    const payload = state.payload;
                    const pLen = payload.length;
                    const lenLo = pLen & 0xFF;
                    const lenHi = (pLen >> 8) & 0xFF;
                    if (repair.payload.length > 0) {
                        rowRhs[0] ^= gfMul(coeff, lenLo);
                    }
                    if (repair.payload.length >= 2) {
                        rowRhs[1] ^= gfMul(coeff, lenHi);
                    }
                    for (let k = 0; k < payload.length; k++) {
                        const idx = 2 + k;
                        if (idx < maxEffLen) {
                            rowRhs[idx] ^= gfMul(coeff, payload[k]);
                        }
                    }
                } else {
                    // Missing: put coefficient in matrix column
                    const pos = binarySearch(missingSeqs, seq >>> 0);
                    if (pos >= 0) rowCoeffs[pos] = coeff;
                }
            }
            coeffMatrix.push(rowCoeffs);
            rhsMatrix.push(rowRhs);
        }

        const solutions = gaussianElim(coeffMatrix, rhsMatrix, nUnknowns, maxEffLen);
        const result: Array<[number, Uint8Array]> = [];
        for (const [col, effPayload] of solutions) {
            const seq = missingSeqs[col];
            if (effPayload.length < 2) continue;
            const payloadLen = effPayload[0] | (effPayload[1] << 8);
            const end = Math.min(2 + payloadLen, effPayload.length);
            result.push([seq, effPayload.slice(2, end)]);
        }
        return result;
    }

    /**
     * Advance highestContiguous and emit LossSpan for permanently missing ranges.
     * Bug-2 fix: start from 0 when highestContiguous is null.
     * Bug-1 fix: emit LossSpan for Missing spans bounded by a known seq.
     */
    private advanceContiguous(): DecoderEvent[] {
        const events: DecoderEvent[] = [];
        outer: for (;;) {
            const next = this.highestContiguous === null
                ? 0
                : (this.highestContiguous + 1) >>> 0;
            const state = this.sources.get(next);

            if (state === undefined) {
                break;
            } else if (state.kind !== 'missing') {
                this.highestContiguous = next;
            } else {
                // Scan forward to find whether the Missing span is bounded
                const spanStart = next;
                let scan = (next + 1) >>> 0;
                for (;;) {
                    const scanState = this.sources.get(scan);
                    if (scanState === undefined) {
                        break outer; // open-ended gap: wait for more symbols
                    } else if (scanState.kind !== 'missing') {
                        // Bounded gap: emit LossSpan and advance
                        events.push({ kind: 'lossSpan', fromSeq: spanStart, toSeqExclusive: scan });
                        this.highestContiguous = (scan - 1 + 0x100000000) >>> 0;
                        continue outer;
                    } else {
                        scan = (scan + 1) >>> 0;
                    }
                }
            }
        }
        return events;
    }
}

// ── Gaussian elimination over GF(256) ────────────────────────────────────

/**
 * Returns [(columnIndex, recoveredVector)] for each uniquely determined pivot.
 * Modifies coeffMatrix and rhsMatrix in-place.
 */
function gaussianElim(
    coeffMatrix: Uint8Array[],
    rhsMatrix: Uint8Array[],
    nUnknowns: number,
    payloadLen: number,
): Array<[number, Uint8Array]> {
    const nRows = coeffMatrix.length;
    if (nRows === 0 || nUnknowns === 0 || payloadLen === 0) return [];

    // pivotRow[col] = row index of pivot for that column, -1 if none
    const pivotRow = new Int32Array(nUnknowns).fill(-1);
    const rowPivotCol = new Int32Array(nRows).fill(-1);
    let currentRow = 0;

    for (let col = 0; col < nUnknowns && currentRow < nRows; col++) {
        // Find pivot in currentRow..nRows for this column
        let p = -1;
        for (let r = currentRow; r < nRows; r++) {
            if (coeffMatrix[r][col] !== 0) { p = r; break; }
        }
        if (p < 0) continue;

        // Swap pivot row to currentRow
        if (p !== currentRow) {
            const tmpC = coeffMatrix[currentRow]; coeffMatrix[currentRow] = coeffMatrix[p]; coeffMatrix[p] = tmpC;
            const tmpR = rhsMatrix[currentRow]; rhsMatrix[currentRow] = rhsMatrix[p]; rhsMatrix[p] = tmpR;
        }

        // Scale pivot row so leading coeff is 1
        const lead = coeffMatrix[currentRow][col];
        const invLead = gfInv(lead);
        const cr = coeffMatrix[currentRow];
        const rr = rhsMatrix[currentRow];
        for (let c = 0; c < nUnknowns; c++) cr[c] = gfMul(cr[c], invLead);
        for (let j = 0; j < payloadLen; j++) rr[j] = gfMul(rr[j], invLead);

        // Eliminate this column from all other rows (full Gauss-Jordan)
        const pivotC = cr.slice(); // clone pivot coeffs
        const pivotR = rr.slice(); // clone pivot rhs
        for (let r = 0; r < nRows; r++) {
            if (r === currentRow) continue;
            const factor = coeffMatrix[r][col];
            if (factor === 0) continue;
            const rowC = coeffMatrix[r];
            const rowR = rhsMatrix[r];
            for (let c = 0; c < nUnknowns; c++) rowC[c] ^= gfMul(factor, pivotC[c]);
            for (let j = 0; j < payloadLen; j++) rowR[j] ^= gfMul(factor, pivotR[j]);
        }

        pivotRow[col] = currentRow;
        rowPivotCol[currentRow] = col;
        currentRow++;
    }

    // Collect uniquely determined solutions
    const pivotCols = new Set<number>();
    for (let c = 0; c < nUnknowns; c++) {
        if (pivotRow[c] >= 0) pivotCols.add(c);
    }

    const result: Array<[number, Uint8Array]> = [];
    for (let col = 0; col < nUnknowns; col++) {
        const pr = pivotRow[col];
        if (pr < 0) continue;
        // Fully determined: all non-pivot columns in this row must be zero
        let ok = true;
        const row = coeffMatrix[pr];
        for (let c = 0; c < nUnknowns && ok; c++) {
            if (!pivotCols.has(c) && row[c] !== 0) ok = false;
        }
        if (ok) result.push([col, rhsMatrix[pr].slice()]);
    }
    return result;
}

// ── Helpers ───────────────────────────────────────────────────────────────

/** Binary search for val in sorted array. Returns index or -1. */
function binarySearch(arr: number[], val: number): number {
    let lo = 0, hi = arr.length - 1;
    while (lo <= hi) {
        const mid = (lo + hi) >>> 1;
        if (arr[mid] === val) return mid;
        if (arr[mid] < val) lo = mid + 1;
        else hi = mid - 1;
    }
    return -1;
}
