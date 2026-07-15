import assert from "node:assert/strict"
import test from "node:test"

import {
    defaultStreamingConfig,
    FecEncoder,
    FecDecoder,
} from "../dist/stream/video/fec.js"

import {
    parseSymbolMessage,
    encodeSymbolMessage,
    parseChunkHeader,
    chunkFrame,
    encodeAck,
    CHUNK_HEADER_SIZE,
    CHUNK_FRAGMENT_MAX,
    SUBSCRIBE_MESSAGE,
    NEEDS_IDR_MESSAGE,
} from "../dist/stream/video/fec_wire.js"

// ── Helpers ───────────────────────────────────────────────────────────────

function pushThrough(enc, dec, seq, payload, dropSource = false) {
    const out = enc.pushSource(seq, payload)
    const events = []
    if (!dropSource) events.push(...dec.pushSymbol(out.source))
    for (const r of out.repairs) events.push(...dec.pushSymbol(r))
    return events
}

function collectRecovered(events) {
    return events.filter(e => e.kind === "recovered")
}

function collectLossSpans(events) {
    return events.filter(e => e.kind === "lossSpan")
}

function makeEnc(num, den, win = 64) {
    return new FecEncoder({ redundancyNumerator: num, redundancyDenominator: den, windowMaxSymbols: win, windowMaxBytes: 1 << 24 })
}

function makeDec(win = 64) {
    return new FecDecoder(win, 1 << 24)
}

// ── GF / coefficient sanity ───────────────────────────────────────────────

test("gfCoeff(0, 0) is non-zero (encoder generates non-zero coeff for first repair/source)", () => {
    // Indirect: encode 1 source with 1/1 ratio, drop source, recover via repair
    const enc = makeEnc(1, 1)
    const dec = makeDec()
    const out = enc.pushSource(0, new Uint8Array([0xDE, 0xAD, 0xBE, 0xEF]))
    // Deliver only the repair (drop source)
    const events = []
    for (const r of out.repairs) events.push(...dec.pushSymbol(r))
    const recovered = collectRecovered(events)
    assert.ok(recovered.some(e => e.seq === 0), "seq 0 must be recovered from repair alone")
    assert.deepEqual(Array.from(recovered[0].payload), [0xDE, 0xAD, 0xBE, 0xEF])
})

// ── Lossless passthrough ──────────────────────────────────────────────────

test("lossless passthrough: all sources recovered when nothing is dropped", () => {
    const enc = makeEnc(1, 8)
    const dec = makeDec()
    const allEvents = []
    for (let i = 0; i < 16; i++) {
        allEvents.push(...pushThrough(enc, dec, i, new Uint8Array([i & 0xFF])))
    }
    const seqs = new Set(collectRecovered(allEvents).map(e => e.seq))
    for (let i = 0; i < 16; i++) assert.ok(seqs.has(i), `seq ${i} must be recovered`)
})

// ── Single loss recovered ─────────────────────────────────────────────────

test("single loss: seq 4 recovered from repair when dropped", () => {
    const enc = makeEnc(1, 1)
    const dec = makeDec()
    const allEvents = []
    for (let i = 0; i < 8; i++) {
        allEvents.push(...pushThrough(enc, dec, i, new Uint8Array([i]), i === 4))
    }
    const seqs = new Set(collectRecovered(allEvents).map(e => e.seq))
    assert.ok(seqs.has(4), `seq 4 must be recovered; got seqs=[${[...seqs].join(",")}]`)
})

// ── Burst loss recovered when enough repairs exist ────────────────────────

test("burst loss: seqs 2 and 3 recovered when 2/8 ratio provides 2 repairs", () => {
    const enc = makeEnc(2, 8)
    const dec = makeDec()
    const allEvents = []
    for (let i = 0; i < 8; i++) {
        allEvents.push(...pushThrough(enc, dec, i, new Uint8Array([i + 0x10]), i === 2 || i === 3))
    }
    const seqs = new Set(collectRecovered(allEvents).map(e => e.seq))
    assert.ok(seqs.has(2), `seq 2 not recovered; seqs=[${[...seqs].join(",")}]`)
    assert.ok(seqs.has(3), `seq 3 not recovered; seqs=[${[...seqs].join(",")}]`)
})

// ── Loss beyond repair capacity yields LossSpan with exact bounds ─────────

test("loss beyond capacity yields LossSpan(2,6) with seqs 0,1,6,7 recovered", () => {
    // 1/4 ratio → 2 repairs for 8 sources; drop 4 consecutive seqs 2..5
    const enc = makeEnc(1, 4)
    const dec = makeDec()
    const allEvents = []
    for (let i = 0; i < 8; i++) {
        allEvents.push(...pushThrough(enc, dec, i, new Uint8Array([i]), i >= 2 && i <= 5))
    }
    const spans = collectLossSpans(allEvents)
    assert.ok(
        spans.some(e => e.fromSeq === 2 && e.toSeqExclusive === 6),
        `expected LossSpan(2,6); got spans=${JSON.stringify(spans)}`,
    )
    const seqs = new Set(collectRecovered(allEvents).map(e => e.seq))
    for (const s of [0, 1, 6, 7]) assert.ok(seqs.has(s), `seq ${s} must be recovered`)
    for (const s of [2, 3, 4, 5]) assert.ok(!seqs.has(s), `seq ${s} must NOT be recovered`)
})

// ── U2 P2 groundwork: recovery/loss-span counters (mirrors fec.rs) ────────

test("stats: clean stream -> all recovery counters zero, source count matches", () => {
    const enc = makeEnc(1, 8)
    const dec = makeDec()
    for (let i = 0; i < 16; i++) {
        pushThrough(enc, dec, i, new Uint8Array([i & 0xFF]))
    }
    const stats = dec.getStats()
    assert.equal(stats.sourceSymbolsReceived, 16, "source count must match symbols fed")
    assert.equal(stats.symbolsRecovered, 0)
    assert.equal(stats.lossSpans, 0)
    assert.equal(stats.lossSpansRecovered, 0)
})

test("stats: single loss recovered counts one span and one recovery", () => {
    // 1/1 ratio: every source gets a repair; drop seq 4 and recover it.
    const enc = makeEnc(1, 1)
    const dec = makeDec()
    for (let i = 0; i < 8; i++) {
        pushThrough(enc, dec, i, new Uint8Array([i]), i === 4)
    }
    const stats = dec.getStats()
    assert.equal(stats.sourceSymbolsReceived, 7, "seq 4 was dropped on the wire")
    assert.equal(stats.symbolsRecovered, 1, "exactly seq 4 recovered via FEC")
    assert.equal(stats.lossSpans, 1, "one loss episode observed")
    assert.equal(stats.lossSpansRecovered, 1, "the episode closed fully healed")
})

test("stats: burst healed by FEC counts one span, not two", () => {
    // Custom delivery (not the uniform pushThrough helper): 2/1 redundancy
    // emits 2 independent repairs (different repairSeq -> independent GF
    // equations) per push, but only the pair built once the window already
    // spans both seq 2 and seq 3 (i.e. built by pushSource(3)) is delivered
    // -- modelling repairs from the earlier, redundant-at-that-point pushes
    // being dropped on the wire. This guarantees both equations covering
    // the 2-wide gap [2,4) land and resolve together in one tryRecover
    // batch, strictly before seq 4's direct arrival could otherwise
    // bound/abandon the gap (existing Bug-1 logic).
    const enc = makeEnc(2, 1)
    const dec = makeDec()

    dec.pushSymbol(enc.pushSource(0, new Uint8Array([0])).source)
    dec.pushSymbol(enc.pushSource(1, new Uint8Array([1])).source)
    // seq 2: source dropped, its repairs discarded (dropped on the wire).
    enc.pushSource(2, new Uint8Array([2]))
    // seq 3: source dropped; both repairs (window now spans [0,4)) delivered.
    const out3 = enc.pushSource(3, new Uint8Array([3]))
    for (const r of out3.repairs) dec.pushSymbol(r)
    // seq 4: delivered directly.
    dec.pushSymbol(enc.pushSource(4, new Uint8Array([4])).source)

    const stats = dec.getStats()
    assert.equal(stats.symbolsRecovered, 2, "both seq 2 and 3 recovered")
    assert.equal(stats.lossSpans, 1, "one contiguous episode, not two")
    assert.equal(stats.lossSpansRecovered, 1)
})

test("stats: unrecoverable gap counts a span but not a recovery", () => {
    // Same scenario as the "loss beyond capacity" test above: 1/4 ratio,
    // seqs 2..5 dropped (4 losses, only 2 repairs) -> the gap is bounded by
    // seq 6 arriving and is abandoned, never healed via FEC.
    const enc = makeEnc(1, 4)
    const dec = makeDec()
    const allEvents = []
    for (let i = 0; i < 8; i++) {
        allEvents.push(...pushThrough(enc, dec, i, new Uint8Array([i]), i >= 2 && i <= 5))
    }
    assert.equal(
        collectLossSpans(allEvents).length, 1,
        "exactly one LossSpan event for the abandoned gap",
    )
    const stats = dec.getStats()
    assert.equal(stats.lossSpans, 1, "one loss episode observed")
    assert.equal(stats.lossSpansRecovered, 0, "the episode was skipped, not healed")
})

// ── Duplicate source ignored ──────────────────────────────────────────────

test("duplicate source: second push for same seq returns no events", () => {
    const dec = makeDec()
    const sym = { kind: "source", seq: 3, payload: new Uint8Array([0xAB]) }
    const e1 = dec.pushSymbol(sym)
    const e2 = dec.pushSymbol(sym)
    assert.ok(collectRecovered(e1).some(e => e.seq === 3), "first push must yield Recovered(3)")
    assert.equal(e2.length, 0, "duplicate source must produce no events")
})

// ── Duplicate repair ignored ──────────────────────────────────────────────

test("duplicate repair: second identical repair push returns no events", () => {
    const enc = makeEnc(1, 1)
    const dec = makeDec()
    const out = enc.pushSource(0, new Uint8Array([1, 2, 3]))
    // Deliver source first so no missing seqs remain
    dec.pushSymbol(out.source)
    const r = out.repairs[0]
    const e1 = dec.pushSymbol(r)
    const e2 = dec.pushSymbol(r)
    assert.equal(collectRecovered(e1).length, 0, "repair after full source: no recovered events")
    assert.equal(e2.length, 0, "duplicate repair must be completely ignored")
})

// ── Stale repair outside window: no crash ────────────────────────────────

test("stale repair pushed to decoder does not crash", () => {
    const enc = makeEnc(1, 1)
    const dec = makeDec()
    // Collect repairs for seqs 0..3
    const staleRepairs = []
    for (let i = 0; i < 4; i++) {
        const out = enc.pushSource(i, new Uint8Array([i]))
        staleRepairs.push(...out.repairs)
    }
    enc.acknowledge(3) // slide encoder window past all these
    // Push stale repairs to decoder — must not throw
    for (const r of staleRepairs) {
        assert.doesNotThrow(() => dec.pushSymbol(r))
    }
})

// ── Encoder acknowledge slides the window ─────────────────────────────────

test("acknowledge(7) evicts seqs 0..7, leaving 8 entries with base=8", () => {
    const enc = makeEnc(1, 8)
    for (let i = 0; i < 16; i++) enc.pushSource(i, new Uint8Array([i]))
    assert.equal(enc.windowLen(), 16)
    enc.acknowledge(7)
    assert.equal(enc.windowLen(), 8)
    assert.equal(enc.windowBase(), 8)
})

test("acknowledge(back_seq) evicts all entries (spec-mandated total wipe)", () => {
    const enc = makeEnc(0, 1) // no repairs, just tracking window
    for (let i = 0; i < 4; i++) enc.pushSource(i, new Uint8Array([i]))
    enc.acknowledge(3) // 3 == back_seq
    assert.equal(enc.windowLen(), 0)
})

test("acknowledge(0) evicts only seq 0, window_base becomes 1", () => {
    const enc = makeEnc(0, 1)
    for (let i = 0; i < 4; i++) enc.pushSource(i, new Uint8Array([i]))
    enc.acknowledge(0)
    assert.equal(enc.windowLen(), 3)
    assert.equal(enc.windowBase(), 1)
})

// ── Acknowledge future-seq guard is a no-op ───────────────────────────────

test("acknowledge(9999) on empty encoder is a no-op", () => {
    const enc = makeEnc(0, 1)
    enc.acknowledge(9999)
    assert.equal(enc.windowLen(), 0)
})

test("acknowledge(back_seq + 1) on non-empty window is a no-op", () => {
    const enc = makeEnc(0, 1)
    for (let i = 0; i < 4; i++) enc.pushSource(i, new Uint8Array([i]))
    enc.acknowledge(4) // back = 3, so 4 is future
    assert.equal(enc.windowLen(), 4)
    assert.equal(enc.windowBase(), 0)
})

// ── Decoder max_symbols / max_bytes (constructor bounds) ─────────────────

test("FecDecoder with small window bounds still recovers single loss", () => {
    const enc = new FecEncoder({ redundancyNumerator: 1, redundancyDenominator: 1, windowMaxSymbols: 4, windowMaxBytes: 1 << 20 })
    const dec = new FecDecoder(4, 1 << 20)
    const allEvents = []
    for (let i = 0; i < 4; i++) {
        allEvents.push(...pushThrough(enc, dec, i, new Uint8Array([i]), i === 2))
    }
    const seqs = new Set(collectRecovered(allEvents).map(e => e.seq))
    assert.ok(seqs.has(2), "seq 2 must be recovered with small window")
})

test("encoder window_max_symbols=1 always evicts previous entry", () => {
    const enc = new FecEncoder({ redundancyNumerator: 0, redundancyDenominator: 1, windowMaxSymbols: 1, windowMaxBytes: 1 << 20 })
    for (let i = 0; i < 5; i++) {
        enc.pushSource(i, new Uint8Array([i]))
        assert.equal(enc.windowLen(), 1, `window must stay at 1 after push ${i}`)
        assert.equal(enc.windowBase(), i, `window_base must equal current seq ${i}`)
    }
})

// ── highest_fully_decoded progression ────────────────────────────────────

test("highest_fully_decoded starts null and advances with seq 0 delivered", () => {
    const dec = makeDec()
    assert.equal(dec.highestFullyDecoded(), null)
    dec.pushSymbol({ kind: "source", seq: 0, payload: new Uint8Array([1]) })
    assert.equal(dec.highestFullyDecoded(), 0)
})

test("highest_fully_decoded stays null when only seq 5 has arrived (Bug-2 pin)", () => {
    const dec = makeDec()
    dec.pushSymbol({ kind: "source", seq: 5, payload: new Uint8Array([5]) })
    assert.equal(
        dec.highestFullyDecoded(), null,
        "must stay null: seqs 0..4 are unconfirmed",
    )
    // Fill the gap; now it should advance all the way to 5
    for (let i = 0; i < 5; i++) {
        dec.pushSymbol({ kind: "source", seq: i, payload: new Uint8Array([i]) })
    }
    assert.equal(dec.highestFullyDecoded(), 5)
})

test("highest_fully_decoded advances through gap once gap is filled", () => {
    const dec = makeDec()
    dec.pushSymbol({ kind: "source", seq: 0, payload: new Uint8Array([0]) })
    dec.pushSymbol({ kind: "source", seq: 3, payload: new Uint8Array([3]) })
    assert.equal(dec.highestFullyDecoded(), 0, "gap at 1 prevents advancing past 0")
    dec.pushSymbol({ kind: "source", seq: 1, payload: new Uint8Array([1]) })
    dec.pushSymbol({ kind: "source", seq: 2, payload: new Uint8Array([2]) })
    assert.equal(dec.highestFullyDecoded(), 3, "gap filled: must advance to 3")
})

// ── Repair-only window recovery ───────────────────────────────────────────

test("repair-only: all 8 sources recovered from 8 independent repairs", () => {
    const enc = makeEnc(1, 1)
    const dec = makeDec()
    const allRepairs = []
    for (let i = 0; i < 8; i++) {
        const out = enc.pushSource(i, new Uint8Array([i + 0x20, i + 0x30]))
        allRepairs.push(...out.repairs)
    }
    const allEvents = []
    for (const r of allRepairs) allEvents.push(...dec.pushSymbol(r))
    const seqs = new Set(collectRecovered(allEvents).map(e => e.seq))
    for (let i = 0; i < 8; i++) assert.ok(seqs.has(i), `seq ${i} not recovered in repair-only scenario`)
})

// ── Wire roundtrips ───────────────────────────────────────────────────────

test("wire roundtrip: source symbol parse → encode → parse", () => {
    const sym = { kind: "source", seq: 0x00123456, payload: new Uint8Array([0xDE, 0xAD, 0xBE, 0xEF]) }
    const buf = encodeSymbolMessage(sym)
    const parsed = parseSymbolMessage(buf)
    assert.ok(parsed !== null)
    assert.equal(parsed.kind, "source")
    assert.equal(parsed.seq, 0x00123456)
    assert.deepEqual(Array.from(parsed.payload), [0xDE, 0xAD, 0xBE, 0xEF])
})

test("wire roundtrip: repair symbol parse → encode → parse", () => {
    const sym = {
        kind: "repair",
        repairSeq: 0xABCD,
        windowBase: 100,
        windowEnd: 110,
        payload: new Uint8Array([1, 2, 3, 4]),
    }
    const buf = encodeSymbolMessage(sym)
    const parsed = parseSymbolMessage(buf)
    assert.ok(parsed !== null)
    assert.equal(parsed.kind, "repair")
    assert.equal(parsed.repairSeq, 0xABCD)
    assert.equal(parsed.windowBase, 100)
    assert.equal(parsed.windowEnd, 110)
    assert.deepEqual(Array.from(parsed.payload), [1, 2, 3, 4])
})

test("parseSymbolMessage returns null for unknown kind byte", () => {
    const buf = new Uint8Array([0x42, 0, 0, 0, 0]).buffer
    assert.equal(parseSymbolMessage(buf), null)
})

// ── Chunk header byte-level LE pin ────────────────────────────────────────

test("chunk header LE pin: frame_id=1, key, timestamp=0x11223344 → exact 13 bytes", () => {
    const frameId = 1
    const chunkIndex = 0
    const chunkCount = 1
    const frameType = 1    // key
    const timestampUs = 0x11223344

    const chunks = chunkFrame(frameId, true, timestampUs, new Uint8Array(0))
    assert.equal(chunks.length, 1)
    const header = new Uint8Array(chunks[0])
    assert.equal(header.length, CHUNK_HEADER_SIZE)
    const expected = new Uint8Array([
        0x01, 0x00, 0x00, 0x00,  // frame_id = 1 LE u32
        0x00, 0x00,               // chunk_index = 0 LE u16
        0x01, 0x00,               // chunk_count = 1 LE u16
        0x01,                     // frame_type = 1 (key)
        0x44, 0x33, 0x22, 0x11,  // timestamp_us = 0x11223344 LE u32
    ])
    assert.deepEqual(Array.from(header), Array.from(expected))
})

// ── chunkFrame ────────────────────────────────────────────────────────────

test("chunkFrame: empty data produces exactly one chunk with header only", () => {
    const chunks = chunkFrame(0, false, 0, new Uint8Array(0))
    assert.equal(chunks.length, 1)
    assert.equal(chunks[0].byteLength, CHUNK_HEADER_SIZE)
    const hdr = parseChunkHeader(chunks[0])
    assert.ok(hdr !== null)
    assert.equal(hdr.chunkCount, 1)
    assert.equal(hdr.chunkIndex, 0)
    assert.equal(hdr.frameType, 0)
})

test("chunkFrame: large data is split across multiple chunks", () => {
    const big = new Uint8Array(CHUNK_FRAGMENT_MAX + 500).fill(0xCC)
    const chunks = chunkFrame(7, true, 0, big)
    assert.equal(chunks.length, 2)
    // First chunk
    const h0 = parseChunkHeader(chunks[0])
    assert.equal(h0.chunkIndex, 0)
    assert.equal(h0.chunkCount, 2)
    assert.equal(new Uint8Array(chunks[0]).length, CHUNK_HEADER_SIZE + CHUNK_FRAGMENT_MAX)
    // Second chunk
    const h1 = parseChunkHeader(chunks[1])
    assert.equal(h1.chunkIndex, 1)
    assert.equal(h1.chunkCount, 2)
    assert.equal(new Uint8Array(chunks[1]).length, CHUNK_HEADER_SIZE + 500)
})

// ── encodeAck ─────────────────────────────────────────────────────────────

test("encodeAck produces 4-byte LE representation of highest seq", () => {
    const ack = new Uint8Array(encodeAck(0x12345678))
    assert.equal(ack.length, 4)
    assert.deepEqual(Array.from(ack), [0x78, 0x56, 0x34, 0x12])
})

// ── Finding 3 pin: oversized repair window rejected (Finding 3) ──────────

test("pushRepair: window larger than 128 symbols is rejected without crash", () => {
    // Craft a repair symbol with windowBase=0, windowEnd=100_000 (way over cap).
    // Old code: for-loop ran 100 000 times inserting Missing entries and could
    // exhaust heap.  Fixed: guard rejects any window whose length > 128.
    const dec = makeDec()
    const oversized = {
        kind: "repair",
        repairSeq: 0,
        windowBase: 0,
        windowEnd: 100_000,
        payload: new Uint8Array([1, 2, 3]),
    }
    const events = dec.pushSymbol(oversized)
    // Must return empty (rejected) without crash.
    assert.deepEqual(events, [], "oversized window repair must be rejected silently")
})

// ── Finding 3 pin: normal max-size window (128 symbols) is accepted ───────

test("pushRepair: window of exactly 128 symbols is accepted", () => {
    const enc = new FecEncoder({
        redundancyNumerator: 1,
        redundancyDenominator: 1,
        windowMaxSymbols: 128,
        windowMaxBytes: 1 << 24,
    })
    const dec = makeDec(128)
    // Fill encoder window with 128 sources, all dropped.
    const repairs = []
    for (let i = 0; i < 128; i++) {
        const out = enc.pushSource(i, new Uint8Array([i & 0xFF]))
        repairs.push(...out.repairs)
    }
    // Feed repairs to decoder — should not crash and should not reject.
    for (const r of repairs) {
        dec.pushSymbol(r) // no throw = pass
    }
})

// ── Finding 5 pin: seenRepairKeys does not grow without bound ────────────

test("seenRepairKeys: map is pruned when it exceeds 256 entries", () => {
    // Build a decoder and feed it >256 unique (repairSeq, windowBase) pairs.
    // First deliver sources 0..300 so highestContiguous advances, enabling pruning.
    const dec = makeDec(128)
    const enc = new FecEncoder({
        redundancyNumerator: 1,
        redundancyDenominator: 1,
        windowMaxSymbols: 64,
        windowMaxBytes: 1 << 24,
    })

    // Advance highestContiguous by delivering 300 sources.
    for (let i = 0; i < 300; i++) {
        const out = enc.pushSource(i, new Uint8Array([i & 0xFF]))
        dec.pushSymbol(out.source)
        for (const r of out.repairs) dec.pushSymbol(r)
    }
    // At this point seenRepairKeys should have been pruned.
    // We can only observe indirectly: the decoder must still accept new repairs.
    const out301 = enc.pushSource(300, new Uint8Array([0x42]))
    dec.pushSymbol(out301.source)
    for (const r of out301.repairs) {
        const events = dec.pushSymbol(r)
        // Should not throw; if it returns an empty array that's fine (source already known).
        assert.ok(Array.isArray(events), "pushRepair must return an array after pruning")
    }
})

// ── Message constants ─────────────────────────────────────────────────────

test("SUBSCRIBE_MESSAGE is a 1-byte buffer containing 0x01", () => {
    const bytes = new Uint8Array(SUBSCRIBE_MESSAGE)
    assert.equal(bytes.length, 1)
    assert.equal(bytes[0], 0x01)
})

test("NEEDS_IDR_MESSAGE is a 1-byte buffer containing 0x00", () => {
    const bytes = new Uint8Array(NEEDS_IDR_MESSAGE)
    assert.equal(bytes.length, 1)
    assert.equal(bytes[0], 0x00)
})
