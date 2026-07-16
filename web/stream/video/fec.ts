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
/**
 * RFC 1982 ordering for u32 serial numbers.
 *
 * `1` means `a` is newer than `b`, `-1` means older, and `null` is the
 * deliberately unordered exact half-range case.
 */
export function compareU32Serial(a: number, b: number): -1 | 0 | 1 | null {
    const distance = ((a >>> 0) - (b >>> 0)) >>> 0;
    if (distance === 0) return 0;
    if (distance === 0x80000000) return null;
    return distance < 0x80000000 ? 1 : -1;
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
    viaFec: boolean;   // true if reconstructed via FEC decode, false if directly received
};

export type LossSpanEvent = {
    kind: 'lossSpan';
    fromSeq: number;          // u32
    toSeqExclusive: number;   // u32
};

export type EvictedEvent = {
    kind: 'evicted';
    fromSeq: number;
    toSeqExclusive: number;
};

export type DecoderEvent = RecoveredEvent | LossSpanEvent | EvictedEvent;

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
        // An ACK after the newest emitted serial, or exactly half a serial
        // space away (RFC 1982 undefined ordering), cannot slide this window.
        const ackVsBack = compareU32Serial(hfd, backSeq);
        if (ackVsBack === 1 || ackVsBack === null) return;

        // The FEC window is insertion ordered, including across u32 wrap.
        // Remove only its acknowledged prefix; never use numeric ordering here.
        while (this.window.length > 0) {
            const entryVsAck = compareU32Serial(this.window[0].seq, hfd);
            if (entryVsAck !== -1 && entryVsAck !== 0) break;
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

// ── U2 P2 groundwork: recovery/loss-span counters ──────────────────────────
//
// Pure instrumentation for a future Gate-B rig to measure recovery rate vs
// redundancy ratio (docs/design/fec-framing.md §8: "Recovered/LossSpan
// counters"). No behavioural effect: with the returned snapshot ignored,
// decoder output (events, highestFullyDecoded) is byte-identical to before
// these counters existed. Rust mirror: fec.rs FecDecoderStats.
//
// Chosen seams (each counter incremented at exactly one authoritative
// point), matching transport-core/src/fec.rs exactly:
// - sourceSymbolsReceived — pushSource, at the point a new (non-duplicate)
//   source payload is recorded as 'received'.
// - repairSymbolsReceived — pushRepair, right after the dedup +
//   oversized-window guards, where the repair is committed to seenRepairKeys.
// - symbolsRecovered — tryRecover, once per (seq, payload) pair a
//   Gaussian-elimination pass resolves.
// - lossSpans / lossSpansRecovered — advanceContiguous / noteOpenSpanAtFrontier
//   / closeOpenSpanStep; see their doc comments for the exact (approximated)
//   span-tracking algorithm.
export interface FecDecoderStats {
    sourceSymbolsReceived: number;
    repairSymbolsReceived: number;
    symbolsRecovered: number;
    lossSpans: number;
    lossSpansRecovered: number;
}

/** Exact persistent-state accounting. `retainedBytes` is source payload bytes
 * + 8/source, repair payload bytes + 16/repair, and 8/dedup key. */
export interface FecDecoderAccounting {
    sourceSymbols: number;
    repairSymbols: number;
    dedupKeys: number;
    /** Largest active matrix dimension; source and repair dimensions are individually capped. */
    algebraSymbols: number;
    retainedBytes: number;
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
    private readonly maxSymbols: number;
    private readonly maxBytes: number;
    private sources = new Map<number, SourceState>();
    private repairs: StoredRepair[] = [];
    // Maps `${repairSeq},${windowBase}` → windowBase (number) for lazy pruning.
    // Value stored as a number so we can evict stale entries without a reverse map.
    private seenRepairKeys = new Map<string, number>();
    private highestContiguous: number | null = null; // null = None
    private stats: FecDecoderStats = {
        sourceSymbolsReceived: 0,
        repairSymbolsReceived: 0,
        symbolsRecovered: 0,
        lossSpans: 0,
        lossSpansRecovered: 0,
    };
    // Start seq of the loss span currently open (blocking highestContiguous
    // advancement) if one has already been counted, else null. See
    // advanceContiguous / noteOpenSpanAtFrontier / closeOpenSpanStep.
    private openLossSpan: number | null = null;
    // Whether at least one seq resolved so far within openLossSpan was a
    // genuine FEC recovery ('recovered') rather than a late direct arrival
    // ('received').
    private openLossSpanHasRecovery = false;
    private newestSeq: number | null = null;
    // `(windowBase, windowEnd)` of the most recently committed forward repair,
    // i.e. the active retained/frontier horizon that admission has already
    // rolled state up to. `null` until the first repair commits — before that,
    // nothing has been retired, so any window is trivially forward. Tracking
    // both endpoints (not just windowBase) rejects a delayed, same-or-earlier
    // -base repair whose windowEnd does not extend past what has already been
    // committed, e.g. a stale [50, 60) arriving after [50, 110) has committed.
    private rollHorizon: [number, number] | null = null;

    constructor(maxSymbols: number, maxBytes: number) {
        this.maxSymbols = Math.min(Math.max(maxSymbols, 1), 128);
        this.maxBytes = Math.min(Math.max(maxBytes, 1), 16 * 1024 * 1024);
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

    /** Snapshot of recovery/loss-span counters accumulated so far. Returns a
     * fresh copy; safe to call at any time. */
    getStats(): FecDecoderStats {
        return { ...this.stats };
    }

    getAccounting(): FecDecoderAccounting {
        let sourceBytes = 0;
        for (const state of this.sources.values()) {
            sourceBytes += state.kind === 'missing' ? 8 : state.payload.byteLength + 8;
        }
        let repairBytes = 0;
        for (const repair of this.repairs) repairBytes += repair.payload.byteLength + 16;
        return {
            sourceSymbols: this.sources.size,
            repairSymbols: this.repairs.length,
            dedupKeys: this.seenRepairKeys.size,
            algebraSymbols: Math.max(this.sources.size, this.repairs.length),
            retainedBytes: sourceBytes + repairBytes + this.seenRepairKeys.size * 8,
        };
    }

    private noteNewest(seq: number): void {
        if (this.newestSeq === null || (((seq - this.newestSeq) >>> 0) < 0x80000000)) {
            this.newestSeq = seq >>> 0;
        }
    }

    /**
     * Evict retained state until adding the specified accounting deltas fits.
     * `protectedSources` are part of the pending admission and must not be
     * selected: otherwise an at-cap arrival at the contiguous frontier can be
     * deleted before advanceContiguous observes it.
     */
    private makeRoom(
        addedSources: number,
        addedRepairs: number,
        addedKeys: number,
        addedBytes: number,
        protectedSources: ReadonlySet<number>,
    ): DecoderEvent[] {
        const events: DecoderEvent[] = [];
        for (;;) {
            const accounting = this.getAccounting();
            const overSymbols = accounting.sourceSymbols + addedSources > this.maxSymbols
                || accounting.repairSymbols + addedRepairs > this.maxSymbols;
            const overBytes = accounting.retainedBytes + addedBytes + addedKeys * 8 > this.maxBytes;
            if (!overSymbols && !overBytes) break;
            if (this.repairs.length > 0
                && (accounting.repairSymbols + addedRepairs > this.maxSymbols || overBytes)) {
                this.repairs.shift();
                this.seenRepairKeys.clear();
                for (const repair of this.repairs) {
                    this.seenRepairKeys.set(`${repair.repairSeq},${repair.windowBase}`, repair.windowBase);
                }
                continue;
            }
            const newest = this.newestSeq;
            if (newest === null) break;
            const frontier = this.highestContiguous ?? newest;
            let oldest: number | null = null;
            let oldestAge = -1;
            // Prefer entries behind the contiguous frontier; all serial
            // comparisons use RFC1982 half-range arithmetic.
            for (const seq of this.sources.keys()) {
                if (protectedSources.has(seq)) continue;
                const age = (frontier - seq) >>> 0;
                if (age < 0x80000000 && age > oldestAge) {
                    oldest = seq;
                    oldestAge = age;
                }
            }
            if (oldest === null) {
                for (const seq of this.sources.keys()) {
                    if (protectedSources.has(seq)) continue;
                    const age = (newest - seq) >>> 0;
                    if (age > oldestAge) {
                        oldest = seq;
                        oldestAge = age;
                    }
                }
            }
            if (oldest === null) break;
            const state = this.sources.get(oldest);
            this.sources.delete(oldest);
            if (state?.kind === 'missing') {
                this.advanceLossFloor(oldest);
                events.push({ kind: 'evicted', fromSeq: oldest, toSeqExclusive: (oldest + 1) >>> 0 });
            }
        }
        return events;
    }

    /** Whether eviction of all non-protected state could admit the deltas. */
    private canMakeRoom(
        addedSources: number,
        addedRepairs: number,
        addedKeys: number,
        addedBytes: number,
        protectedSources: ReadonlySet<number>,
    ): boolean {
        const accounting = this.getAccounting();
        let sourceSymbols = accounting.sourceSymbols;
        let retainedBytes = accounting.retainedBytes;
        for (const repair of this.repairs) {
            retainedBytes -= repair.payload.byteLength + 16;
        }
        retainedBytes -= this.seenRepairKeys.size * 8;
        for (const [seq, state] of this.sources) {
            if (protectedSources.has(seq)) continue;
            sourceSymbols--;
            retainedBytes -= state.kind === 'missing' ? 8 : state.payload.byteLength + 8;
        }
        return sourceSymbols + addedSources <= this.maxSymbols
            && addedRepairs <= this.maxSymbols
            && retainedBytes + addedBytes + addedKeys * 8 <= this.maxBytes;
    }

    private pushSource(seq: number, payload: Uint8Array): DecoderEvent[] {
        const s = seq >>> 0;
        if (payload.byteLength + 8 > this.maxBytes) {
            return [{ kind: 'recovered', seq: s, payload, viaFec: false }];
        }
        const existing = this.sources.get(s);
        if (existing !== undefined && existing.kind !== 'missing') {
            return []; // already Received or Recovered
        }

        const addedSources = existing === undefined ? 1 : 0;
        const addedBytes = existing === undefined ? payload.byteLength + 8 : payload.byteLength;
        const protectedSources = new Set([s]);
        if (!this.canMakeRoom(addedSources, 0, 0, addedBytes, protectedSources)) {
            return [{ kind: 'recovered', seq: s, payload, viaFec: false }];
        }

        this.noteNewest(s);
        const events = this.makeRoom(addedSources, 0, 0, addedBytes, protectedSources);
        this.noteOpenSpanAtFrontier();
        this.sources.set(s, { kind: 'received', payload });
        this.stats.sourceSymbolsReceived++;
        events.push({ kind: 'recovered', seq: s, payload, viaFec: false });
        const recovered = this.tryRecover();
        for (const [rs, rp] of recovered) {
            events.push({ kind: 'recovered', seq: rs, payload: rp, viaFec: true });
        }
        events.push(...this.advanceContiguous());
        return events;
    }

    /** Classify a repair against `rollHorizon` before touching any state.
     * 'stale': windowEnd is serially older than the committed end, or equal
     * to it with a different windowBase (a same-or-narrower sub-window that
     * gains no new coverage).
     * 'forward': windowEnd is serially newer than the committed end AND
     * windowBase is not serially older than the committed base, or the
     * window is identical to the committed one.
     * 'overlapping': windowEnd is serially newer but windowBase is serially
     * older than the committed base — would need equations for
     * already-retired state, so it cannot be admitted. */
    private repairHorizonRelation(windowBase: number, windowEnd: number): 'stale' | 'overlapping' | 'forward' {
        if (this.rollHorizon === null) return 'forward';
        const [horizonBase, horizonEnd] = this.rollHorizon;
        const endCmp = compareU32Serial(windowEnd, horizonEnd);
        if (endCmp === null || endCmp === -1) return 'stale';
        if (endCmp === 0) return windowBase === horizonBase ? 'forward' : 'stale';
        const baseCmp = compareU32Serial(windowBase, horizonBase);
        return (baseCmp === 1 || baseCmp === 0) ? 'forward' : 'overlapping';
    }

    private pushRepair(
        repairSeq: number, windowBase: number, windowEnd: number, payload: Uint8Array,
    ): DecoderEvent[] {
        const wb = windowBase >>> 0;
        const we = windowEnd >>> 0;
        if (payload.byteLength + 16 > this.maxBytes) return [];

        // Guard: reject repair symbols with windows larger than the design cap of
        // 128 symbols (design §4).  A window of e.g. 100 000 would insert 100 000
        // Missing entries and stall the event loop — this is an off-spec or
        // adversarially crafted symbol.
        const windowLen = (we - wb + 0x100000000) >>> 0;
        if (windowLen > 128) return [];

        // Do not let an unseen historical equation roll the retained horizon
        // backward. An overlap whose prefix has already been retired cannot be
        // used safely: elimination would otherwise treat that prefix as zero.
        if (this.repairHorizonRelation(wb, we) !== 'forward') return [];

        // Deduplicate by (repairSeq, windowBase) — full u16 repairSeq (Bug-3 fix)
        const key = `${repairSeq & 0xFFFF},${wb}`;
        if (this.seenRepairKeys.has(key)) return [];

        const protectedSources = new Set<number>();
        let addedSources = 0;
        for (let seq = wb; seq !== we; seq = (seq + 1) >>> 0) {
            protectedSources.add(seq);
            if (!this.sources.has(seq)) addedSources++;
        }
        const addedBytes = addedSources * 8 + payload.byteLength + 16;
        if (!this.canMakeRoom(addedSources, 1, 1, addedBytes, protectedSources)) return [];

        if (windowLen !== 0) {
            this.noteNewest((we - 1 + 0x100000000) >>> 0);
            // Advance the declared rolling horizon to this repair's base/end
            // now that admission is known to fit.
            this.rollHorizon = [wb, we];
        }
        const events = this.makeRoom(addedSources, 1, 1, addedBytes, protectedSources);

        // Commit dedup key only after the complete repair admission is known to fit.
        this.seenRepairKeys.set(key, wb);
        this.stats.repairSymbolsReceived++;

        // Register all seqs in this repair's window as at least Missing.
        for (let seq = wb; seq !== we; seq = (seq + 1) >>> 0) {
            if (!this.sources.has(seq)) {
                this.sources.set(seq, { kind: 'missing' });
            }
        }

        this.noteOpenSpanAtFrontier();
        this.repairs.push({ repairSeq: repairSeq & 0xFFFF, windowBase: wb, windowEnd: we, payload });

        const recovered = this.tryRecover();
        events.push(...recovered.map(
            ([s, p]) => ({ kind: 'recovered' as const, seq: s, payload: p, viaFec: true }),
        ));
        events.push(...this.advanceContiguous());

        return events;
    }

    /** Iteratively recover Missing symbols via Gaussian elimination. */
    private tryRecover(): Array<[number, Uint8Array]> {
        const allRecovered: Array<[number, Uint8Array]> = [];
        for (;;) {
            const batch = this.oneElimPass();
            if (batch.length === 0) break;
            const recoveredBytes = batch.reduce((total, [, payload]) => total + payload.byteLength, 0);
            if (this.getAccounting().retainedBytes + recoveredBytes > this.maxBytes) break;
            this.stats.symbolsRecovered += batch.length;
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
        if (nUnknowns > 128 || this.repairs.length > 128) return [];
        const scratch = this.repairs.length * (nUnknowns + maxEffLen)
            + 2 * nUnknowns + 2 * maxEffLen;
        if (this.getAccounting().retainedBytes + scratch > this.maxBytes) return [];
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
     * Advance highestContiguous, emit LossSpan for permanently missing
     * ranges, and update the loss-span counters (stats.lossSpans,
     * stats.lossSpansRecovered).
     *
     * Bug-2 fix: start from 0 when highestContiguous is null.
     * Bug-1 fix: emit LossSpan for Missing spans bounded by a known seq.
     *
     * Loss-span counter approximation (U2 P2 groundwork, mirrors fec.rs):
     * this decoder has no independent structure tracking arbitrary loss
     * episodes. Bounded Missing eviction is instead recorded as an explicit
     * loss-floor transition; other gaps are discovered here (plus
     * noteOpenSpanAtFrontier for the instant-resolution case — see below).
     * Spans are counted lazily, at the moment the contiguous frontier
     * reaches them, not the instant a repair's window first registers a seq
     * as 'missing'. Because `next` is always exactly `highestContiguous+1`,
     * at most one span can ever be "open" (blocking the frontier) at a
     * time, so `openLossSpan`/`openLossSpanHasRecovery` need only track a
     * single in-flight span.
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
            } else if (state.kind === 'received') {
                this.closeOpenSpanStep(next, false);
                this.highestContiguous = next;
            } else if (state.kind === 'recovered') {
                this.closeOpenSpanStep(next, true);
                this.highestContiguous = next;
            } else {
                if (this.openLossSpan === null) {
                    this.stats.lossSpans++;
                    this.openLossSpan = next;
                    this.openLossSpanHasRecovery = false;
                }
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
                        this.openLossSpan = null;
                        this.openLossSpanHasRecovery = false;
                        continue outer;
                    } else {
                        scan = (scan + 1) >>> 0;
                    }
                }
            }
        }
        return events;
    }
    /**
     * A forced Missing eviction is an irreversible loss decision. Move the
     * acknowledgement floor through it so a deleted frontier cannot leave
     * advanceContiguous waiting for a source that will never reappear.
     */
    private advanceLossFloor(seq: number): void {
        const lossSeq = seq >>> 0;
        if (this.openLossSpan === null) this.stats.lossSpans++;
        this.openLossSpan = null;
        this.openLossSpanHasRecovery = false;

        if (this.highestContiguous === null
            || compareU32Serial(lossSeq, this.highestContiguous) === 1) {
            this.highestContiguous = lossSeq;
        }
    }

    /**
     * If the frontier (highestContiguous + 1, or 0 if unset) is already
     * 'missing' and no span is currently open, open one and count
     * stats.lossSpans++.
     *
     * Called explicitly at the top of pushSource (before the seq being
     * pushed can resolve the frontier) and after the Missing-registration
     * loop in pushRepair (before tryRecover can cascade-resolve it). This
     * is necessary because tryRecover runs *within* the same
     * pushSource/pushRepair call that can also be the one supplying the
     * last missing piece of a gap — the common "one symbol lost, one repair
     * received" case resolves the frontier before advanceContiguous ever
     * gets a chance to observe it as 'missing'. advanceContiguous keeps its
     * own (guarded, no-op-if-already-open) span-open check for the
     * complementary case: a gap that was registered 'missing' earlier and
     * only reached once the frontier advances up to it on a later call.
     */
    private noteOpenSpanAtFrontier(): void {
        if (this.openLossSpan !== null) return;
        const frontier = this.highestContiguous === null
            ? 0
            : (this.highestContiguous + 1) >>> 0;
        const state = this.sources.get(frontier);
        if (state !== undefined && state.kind === 'missing') {
            this.stats.lossSpans++;
            this.openLossSpan = frontier;
            this.openLossSpanHasRecovery = false;
        }
    }

    /**
     * Loss-span bookkeeping for one seq resolving to 'received'/'recovered'
     * while advanceContiguous walks the frontier. No-op when no span is
     * currently open. Closes the open span (counting lossSpansRecovered
     * when healed) once the seq immediately after resolvedSeq is no longer
     * 'missing', i.e. the whole originally-contiguous run has been
     * consumed.
     */
    private closeOpenSpanStep(resolvedSeq: number, viaFec: boolean): void {
        if (this.openLossSpan === null) return;
        if (viaFec) this.openLossSpanHasRecovery = true;
        const nextState = this.sources.get((resolvedSeq + 1) >>> 0);
        const stillMissing = nextState !== undefined && nextState.kind === 'missing';
        if (!stillMissing) {
            if (this.openLossSpanHasRecovery) this.stats.lossSpansRecovered++;
            this.openLossSpan = null;
            this.openLossSpanHasRecovery = false;
        }
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
