import assert from "node:assert/strict"
import test from "node:test"

import { compareU32Serial, defaultStreamingConfig, FecEncoder, FecDecoder } from "../dist/stream/video/fec.js"
import {
    parseSymbolMessage, encodeSymbolMessage, encodeSymbolMessageV2,
    parseChunkHeader, parseChunkHeaderV2, chunkFrame, chunkFrameV2, encodeAck,
    encodeAckV2, encodeSubscribeV2, encodeNeedsIdrV2, parseControlMessage,
    CHUNK_HEADER_SIZE, CHUNK_FRAGMENT_MAX, CHUNK_V2_FRAGMENT_MAX, SOURCE_MESSAGE_MAX, SOURCE_PAYLOAD_MAX,
    REPAIR_PAYLOAD_MAX, REPAIR_MESSAGE_MAX, FRAME_MAX_BYTES, SUBSCRIBE_MESSAGE, NEEDS_IDR_MESSAGE,
} from "../dist/stream/video/fec_wire.js"
import { FecDecodePipe } from "../dist/stream/video/fec_decode_pipe.js"
import { resolveFecCapability, handleFecDataMessage } from "../dist/stream/video/fec_session.js"

// ── Helpers ───────────────────────────────────────────────────────────────

/** Minimal DataVideoRenderer that captures submitDecodeUnit calls. */
function makeRenderer() {
    const units = []
    const renderer = {
        implementationName: "test-renderer",
        units,
        submitDecodeUnit(unit) { units.push(unit) },
        getBase() { return null },
    }
    return renderer
}

/** Build a chunk payload for a frame. */
function buildChunkPayload(frameId, chunkIndex, chunkCount, frameType, timestampUs, fragment) {
    const buf = new ArrayBuffer(CHUNK_HEADER_SIZE + fragment.length)
    const view = new DataView(buf)
    const bytes = new Uint8Array(buf)
    view.setUint32(0, frameId, true)
    view.setUint16(4, chunkIndex, true)
    view.setUint16(6, chunkCount, true)
    view.setUint8(8, frameType)
    view.setUint32(9, timestampUs, true)
    bytes.set(fragment, CHUNK_HEADER_SIZE)
    return new Uint8Array(buf)
}

/** Wrap a chunk payload as a source-symbol wire message. */
function sourceMsg(seq, chunkPayload) {
    const buf = new ArrayBuffer(5 + chunkPayload.length)
    const view = new DataView(buf)
    const bytes = new Uint8Array(buf)
    view.setUint8(0, 0)
    view.setUint32(1, seq, true)
    bytes.set(chunkPayload, 5)
    return buf
}

/** Wrap a repair symbol as wire message. */
function repairMsg(sym) {
    return encodeSymbolMessage(sym)
}

function makeEnc(num = 1, den = 1, win = 64) {
    return new FecEncoder({
        redundancyNumerator: num,
        redundancyDenominator: den,
        windowMaxSymbols: win,
        windowMaxBytes: 1 << 24,
    })
}

// ── Test: out-of-order chunk arrival reassembles byte-identical frame ─────

test("out-of-order chunks reassemble byte-identical frame", () => {
    const renderer = makeRenderer()
    const pipe = new FecDecodePipe(renderer)

    const frameData = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8])
    const frag1 = frameData.slice(0, 4)
    const frag2 = frameData.slice(4, 8)

    const chunk0 = buildChunkPayload(0, 0, 2, 0, 1000, frag1)
    const chunk1 = buildChunkPayload(0, 1, 2, 0, 1000, frag2)

    // Submit chunk 1 first, then chunk 0
    pipe.submitPacket(sourceMsg(1, chunk1))
    assert.equal(renderer.units.length, 0, "not yet: missing chunk 0")

    pipe.submitPacket(sourceMsg(0, chunk0))
    assert.equal(renderer.units.length, 1, "frame assembled after second chunk")

    const unit = renderer.units[0]
    const assembled = new Uint8Array(unit.data)
    assert.deepEqual(assembled, frameData)
    assert.equal(unit.type, "delta")
    assert.equal(unit.timestampMicroseconds, 1000)
})

// ── Test: two frames interleaved both deliver ─────────────────────────────

test("two frames interleaved chunks both deliver", () => {
    const renderer = makeRenderer()
    const pipe = new FecDecodePipe(renderer)

    const data0 = new Uint8Array([10, 20])
    const data1 = new Uint8Array([30, 40])

    // frame 0 chunk 0, frame 1 chunk 0, frame 0 chunk 1, frame 1 chunk 1
    const f0c0 = buildChunkPayload(0, 0, 2, 0, 100, data0.slice(0, 1))
    const f1c0 = buildChunkPayload(1, 0, 2, 1, 200, data1.slice(0, 1))
    const f0c1 = buildChunkPayload(0, 1, 2, 0, 100, data0.slice(1, 2))
    const f1c1 = buildChunkPayload(1, 1, 2, 1, 200, data1.slice(1, 2))

    pipe.submitPacket(sourceMsg(0, f0c0))
    pipe.submitPacket(sourceMsg(1, f1c0))
    pipe.submitPacket(sourceMsg(2, f0c1))
    assert.equal(renderer.units.length, 1)
    pipe.submitPacket(sourceMsg(3, f1c1))
    assert.equal(renderer.units.length, 2)

    assert.deepEqual(new Uint8Array(renderer.units[0].data), data0)
    assert.deepEqual(new Uint8Array(renderer.units[1].data), data1)
    assert.equal(renderer.units[1].type, "key")

    // U2 P2 groundwork: clean stream (no loss) -> all recovery counters
    // zero; source count matches the 4 symbols fed.
    const stats = pipe.getStats()
    assert.equal(stats.sourceSymbolsReceived, 4)
    assert.equal(stats.symbolsRecovered, 0)
    assert.equal(stats.framesRecovered, 0)
    assert.equal(stats.lossSpans, 0)
    assert.equal(stats.lossSpansRecovered, 0)
})

// ── Test: repair-recovered chunk completes a frame ────────────────────────

test("repair recovery: drop one source symbol, repair recovers it, frame assembles", () => {
    // Scenario: frame_id=0 has 2 chunks (seq=0 and seq=1 of chunk_count=2).
    // seq=0 is dropped; only seq=1 (source) and the repair from enc.pushSource(1)
    // (which covers window [0..2)) reach the pipe.  The decoder has 1 unknown
    // (seq=0) and 1 equation (the repair) → Gaussian elimination recovers seq=0.
    // Both chunks are now available, so the frame assembles.
    const renderer = makeRenderer()
    const pipe = new FecDecodePipe(renderer)

    const enc = makeEnc(1, 1, 64)

    const frag0 = new Uint8Array([0xAA, 0xBB])
    const frag1 = new Uint8Array([0xCC, 0xDD])
    const chunk0 = buildChunkPayload(0, 0, 2, 0, 5000, frag0) // seq=0, chunk 0/2
    const chunk1 = buildChunkPayload(0, 1, 2, 0, 5000, frag1) // seq=1, chunk 1/2

    // Feed both to encoder to build the window (drop enc.pushSource(0) repair)
    enc.pushSource(0, chunk0)
    const { repairs: repairsFromSeq1 } = enc.pushSource(1, chunk1)
    // repairsFromSeq1[0] covers window [0..2): can recover seq=0 given seq=1 known

    // Deliver the repair FIRST, then seq=1 (source)
    // After repair: decoder knows seq=0 is Missing, seq=1 is Missing → underdetermined (2 unknowns, 1 eq)
    pipe.submitPacket(repairMsg(repairsFromSeq1[0]))
    assert.equal(renderer.units.length, 0, "no frame yet: 2 unknowns, 1 equation")

    // Deliver seq=1 source: decoder now has 1 unknown (seq=0), 1 equation → recovers seq=0
    // FecDecodePipe receives: recovered(seq=1), recovered(seq=0) → both chunks of frame → assembles
    pipe.submitPacket(sourceMsg(1, chunk1))
    assert.equal(renderer.units.length, 1, "frame assembled via FEC recovery")

    const unit = renderer.units[0]
    const assembled = new Uint8Array(unit.data)
    assert.deepEqual(assembled, new Uint8Array([0xAA, 0xBB, 0xCC, 0xDD]))
    assert.equal(unit.type, "delta")
    assert.equal(unit.timestampMicroseconds, 5000)

    // U2 P2 groundwork: the frame used a recovered symbol (seq 0), and the
    // decoder-level counters passthrough correctly.
    const stats = pipe.getStats()
    assert.equal(stats.framesRecovered, 1, "frame used a recovered symbol")
    assert.equal(stats.symbolsRecovered, 1, "exactly seq 0 recovered via FEC")
    assert.equal(stats.lossSpans, 1)
    assert.equal(stats.lossSpansRecovered, 1)
})

// ── Test: LossSpan drops pending frame, pollRequestIdr returns true once ──


// ── Test: pending-map cap at 8 frames triggers a full-reset discontinuity ──

test("pending-map cap at 8 frames sets needsIdr via full-reset discontinuity", () => {
    const renderer = makeRenderer()
    const pipe = new FecDecodePipe(renderer)

    // Push 8 partial frames (chunk 0 of 2 each — never complete)
    for (let i = 0; i < 8; i++) {
        const chunk = buildChunkPayload(i, 0, 2, 0, i * 1000, new Uint8Array([i]))
        pipe.submitPacket(sourceMsg(i * 2, chunk))
    }
    assert.equal(renderer.units.length, 0)
    assert.equal(pipe.pollRequestIdr(), false, "no eviction yet at exactly 8")

    // Push a 9th partial frame — this trips the cap, which is a full-reset
    // discontinuity (every pending record clears, not just the oldest one).
    const chunk9 = buildChunkPayload(8, 0, 2, 0, 9000, new Uint8Array([9]))
    pipe.submitPacket(sourceMsg(16, chunk9))
    assert.equal(pipe.pollRequestIdr(), true, "full-reset discontinuity at 9th frame sets needsIdr")
    assert.equal(pipe.pollRequestIdr(), false, "cleared after one poll")
})

// ── Test: an incomplete pending delta must never claim the reorder anchor ──

test("key after incomplete earlier delta becomes the reorder anchor immediately", () => {
    const renderer = makeRenderer()
    const discontinuities = []
    const pipe = new FecDecodePipe(renderer, undefined, {
        onDiscontinuity: event => discontinuities.push(event),
    })

    // Trigger a full-reset discontinuity (reassembly-eviction), which
    // latches awaitingIdr the same way any other discontinuity does.
    for (let i = 0; i < 8; i++) {
        const chunk = buildChunkPayload(i, 0, 2, 0, i * 1000, new Uint8Array([i]))
        pipe.submitPacket(sourceMsg(i * 2, chunk))
    }
    const chunk9 = buildChunkPayload(8, 0, 2, 0, 9000, new Uint8Array([9]))
    pipe.submitPacket(sourceMsg(16, chunk9))
    assert.equal(pipe.pollRequestIdr(), true, "discontinuity latches needsIdr")
    assert.equal(discontinuities.length, 1, "exactly one discontinuity so far")

    // WATCH finding agent://76-FinalReviewCore: while awaitingIdr is
    // latched, an incomplete pending delta must never claim nextFrameId.
    // Earlier delta (frame 100, 2 chunks) — only the first chunk ever
    // arrives, so it stays pending/incomplete forever.
    const staleDelta = buildChunkPayload(100, 0, 2, 0, 100_000, new Uint8Array([0xAA]))
    pipe.submitPacket(sourceMsg(20, staleDelta))
    assert.equal(renderer.units.length, 0, "stale incomplete delta does not emit")

    // Later key (frame 101, single chunk) completes in the same message —
    // it must become the post-reset anchor and flush immediately, not wait
    // behind the stale delta's frame id.
    const key = buildChunkPayload(101, 0, 1, 1, 101_000, new Uint8Array([0xBB]))
    pipe.submitPacket(sourceMsg(21, key))
    assert.equal(renderer.units.length, 1, "the key must emit immediately, not wait for a reorder gap")
    assert.equal(renderer.units[0].type, "key")
    assert.equal(discontinuities.length, 1, "no extra ReorderGap/IDR discontinuity from the stale delta")
    assert.equal(
        pipe.pollRequestIdr(),
        false,
        "the recovered key must not trigger a second IDR request"
    )

    // The gate has cleared: the next delta emits immediately too.
    const delta = buildChunkPayload(102, 0, 1, 0, 102_000, new Uint8Array([0xCC]))
    pipe.submitPacket(sourceMsg(22, delta))
    assert.equal(renderer.units.length, 2, "subsequent delta emits once the gate has cleared")
    pipe.dispose()
})

// ── Test: unrecoverable gap counts a loss span but not a recovery ─────────

test("stats: unrecoverable gap increments lossSpans, not lossSpansRecovered", () => {
    // Same scenario as fec.rs's pin_loss_span_emitted_for_bounded_missing_gap
    // / fec_decoder.test.mjs's "unrecoverable gap" stats test: 1/4 ratio,
    // seqs 2..5 dropped (4 losses, only 2 repairs) -> the gap is bounded and
    // abandoned, never healed via FEC. Each seq carries a single-chunk frame.
    const renderer = makeRenderer()
    const pipe = new FecDecodePipe(renderer)
    const enc = makeEnc(1, 4, 64)

    for (let i = 0; i < 8; i++) {
        const chunk = buildChunkPayload(i, 0, 1, 0, i * 1000, new Uint8Array([i]))
        const out = enc.pushSource(i, chunk)
        const drop = i >= 2 && i <= 5
        if (!drop) pipe.submitPacket(sourceMsg(i, chunk))
        for (const r of out.repairs) pipe.submitPacket(repairMsg(r))
    }

    const stats = pipe.getStats()
    assert.equal(stats.lossSpans, 1, "one loss episode observed")
    assert.equal(stats.lossSpansRecovered, 0, "the episode was skipped, not healed")
    // Any unrecoverable source gap invalidates predictive references, even if
    // the lost frame had no pending chunks.
    assert.equal(pipe.pollRequestIdr(), true, "unrecoverable gap requests IDR")
    assert.equal(pipe.pollRequestIdr(), false, "needsIdr cleared after one poll")
})

// ── Test: ACK cadence — 32 symbol trigger ────────────────────────────────

test("ack fires after 32 delivered source symbols", () => {
    const renderer = makeRenderer()
    const acks = []
    let clock = 0

    const pipe = new FecDecodePipe(renderer, undefined, {
        onAck: (h) => acks.push(h),
        now: () => clock,
    })

    // Push 31 single-chunk frames — no ack yet
    for (let i = 0; i < 31; i++) {
        const chunk = buildChunkPayload(i, 0, 1, 0, i * 1000, new Uint8Array([i & 0xFF]))
        pipe.submitPacket(sourceMsg(i, chunk))
    }
    assert.equal(acks.length, 0, "no ack before 32 symbols")

    // 32nd symbol triggers ack
    const chunk32 = buildChunkPayload(31, 0, 1, 0, 31000, new Uint8Array([31]))
    pipe.submitPacket(sourceMsg(31, chunk32))
    assert.equal(acks.length, 1, "ack fires at 32nd symbol")
    assert.equal(acks[0], 31, "ack reports correct highest_fully_decoded")
})

// ── Test: ACK cadence — 50ms timer trigger ───────────────────────────────

test("ack fires after 50ms elapsed even with fewer than 32 symbols", () => {
    const renderer = makeRenderer()
    const acks = []
    let clock = 0

    const pipe = new FecDecodePipe(renderer, undefined, {
        onAck: (h) => acks.push(h),
        now: () => clock,
    })

    // Push 5 symbols, no time elapsed
    for (let i = 0; i < 5; i++) {
        const chunk = buildChunkPayload(i, 0, 1, 0, i * 1000, new Uint8Array([i]))
        pipe.submitPacket(sourceMsg(i, chunk))
    }
    assert.equal(acks.length, 0)

    // Advance clock past 50ms
    clock = 51

    // One more symbol triggers time-based ack
    const chunk5 = buildChunkPayload(5, 0, 1, 0, 5000, new Uint8Array([5]))
    pipe.submitPacket(sourceMsg(5, chunk5))
    assert.equal(acks.length, 1, "ack fires on 50ms elapsed")
    assert.equal(acks[0], 5)
})

// ── Test: timer-driven ACK when no symbols arrive (Finding 2+8) ──────────

test("tickTimer fires ack when hfd advanced and 50ms have elapsed (no symbol required)", () => {
    const renderer = makeRenderer()
    const acks = []
    let clock = 0

    const pipe = new FecDecodePipe(renderer, undefined, {
        onAck: (h) => acks.push(h),
        now: () => clock,
    })

    // Deliver 5 symbols so highestFullyDecoded advances.
    for (let i = 0; i < 5; i++) {
        const chunk = buildChunkPayload(i, 0, 1, 0, i * 1000, new Uint8Array([i]))
        pipe.submitPacket(sourceMsg(i, chunk))
    }
    assert.equal(acks.length, 0, "no ack yet: fewer than 32 symbols and no time elapsed")

    // Advance clock past 50ms — then tickTimer() simulates the independent timer
    // firing without any new symbol arriving.
    clock = 60
    pipe.tickTimer()
    assert.equal(acks.length, 1, "timer-driven ack fires after 50ms even with no new symbol")
    assert.equal(acks[0], 4, "ack reports correct highest_fully_decoded")

    // Second tick with the same hfd — must not re-fire.
    pipe.tickTimer()
    assert.equal(acks.length, 1, "second timer tick with same hfd must not fire duplicate ack")
})

// ── Test: SUBSCRIBE_MESSAGE bytes ─────────────────────────────────────────

test("SUBSCRIBE_MESSAGE is 1-byte 0x01", () => {
    const bytes = new Uint8Array(SUBSCRIBE_MESSAGE)
    assert.equal(bytes.length, 1)
    assert.equal(bytes[0], 0x01)
})

// ── Test: NEEDS_IDR_MESSAGE bytes ────────────────────────────────────────

test("NEEDS_IDR_MESSAGE is 1-byte 0x00", () => {
    const bytes = new Uint8Array(NEEDS_IDR_MESSAGE)
    assert.equal(bytes.length, 1)
    assert.equal(bytes[0], 0x00)
})

// ── Test: encodeAck produces 4-byte little-endian u32 ────────────────────

test("encodeAck encodes highest_fully_decoded as 4-byte LE u32", () => {
    const buf = encodeAck(0x01020304)
    const view = new DataView(buf)
    assert.equal(view.getUint32(0, true), 0x01020304)
    assert.equal(buf.byteLength, 4)
})
test("v2 Subscribe, ACK, and NeedsIdr controls require exact lengths and values", () => {
    const controls = [
        [encodeSubscribeV2(0x01020304), 5, { version: 2, epoch: 0x01020304 }],
        [encodeAckV2(0x01020304, 0xa0b0c0d0), 9, { version: 2, epoch: 0x01020304, highestSeq: 0xa0b0c0d0 }],
        [encodeNeedsIdrV2(0x01020304, 7), 6, { version: 2, epoch: 0x01020304, reason: 7 }],
    ]
    for (const [control, length, expected] of controls) {
        const bytes = new Uint8Array(control)
        assert.equal(bytes.length, length)
        assert.deepEqual(parseControlMessage(control), expected, "parse exact control values")
        assert.equal(parseControlMessage(bytes.slice(0, length - 1).buffer), null, `reject ${length - 1}-byte control`)
        const long = new Uint8Array(length + 1)
        long.set(bytes)
        assert.equal(parseControlMessage(long.buffer), null, `reject ${length + 1}-byte control`)
    }
    assert.deepEqual(Array.from(new Uint8Array(encodeSubscribeV2(0x01020304))), [0x82, 4, 3, 2, 1])
    assert.deepEqual(Array.from(new Uint8Array(encodeAckV2(0x01020304, 0xa0b0c0d0))), [0x81, 4, 3, 2, 1, 0xd0, 0xc0, 0xb0, 0xa0])
    assert.deepEqual(Array.from(new Uint8Array(encodeNeedsIdrV2(0x01020304, 7))), [0x80, 4, 3, 2, 1, 7])
})
test("v2 symbol parser rejects each malformed field without version fallback", () => {
    const validChunk = new Uint8Array(chunkFrameV2(1, false, 2, new Uint8Array([1, 2, 3]))[0])
    const message = new Uint8Array(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 7, seq: 9, payload: validChunk,
    }))
    assert.equal(parseSymbolMessage(message.slice(0, message.length - 1).buffer), null, "truncated v2")
    const corrupt = message.slice()
    corrupt[corrupt.length - 1] ^= 0xff
    assert.equal(parseSymbolMessage(corrupt.buffer), null, "CRC-corrupt v2")
    assert.throws(() => encodeSymbolMessageV2({
        kind: "repair", version: 2, epoch: 7, repairSeq: 1, windowBase: 0, windowEnd: 1,
        payload: new Uint8Array(REPAIR_PAYLOAD_MAX + 1),
    }), /wire cap/)
})
test("v2 source and repair wire caps are pinned to frozen limits", () => {
    assert.equal(SOURCE_MESSAGE_MAX, 1200)
    assert.equal(SOURCE_PAYLOAD_MAX, 1185)
    assert.equal(REPAIR_MESSAGE_MAX, 1221)
    assert.equal(REPAIR_PAYLOAD_MAX, 1200)
    assert.equal(CHUNK_V2_FRAGMENT_MAX, 1164)
    assert.equal(FRAME_MAX_BYTES, 4 * 1024 * 1024)

    const [sourcePayload] = chunkFrameV2(1, false, 0, new Uint8Array(1164))
    const source = encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 7, seq: 1, payload: new Uint8Array(sourcePayload),
    })
    assert.equal(source.byteLength, 1200)
    assert.equal(parseSymbolMessage(source)?.payload.byteLength, 1185)
    assert.throws(() => encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 7, seq: 2,
        payload: new Uint8Array(1186),
    }), /wire cap/)
    const oversizedSource = new Uint8Array(1201)
    oversizedSource.set(new Uint8Array(source))
    new DataView(oversizedSource.buffer).setUint16(9, 1186, true)
    assert.equal(parseSymbolMessage(oversizedSource.buffer), null)

    const repair = {
        kind: "repair", version: 2, epoch: 7, repairSeq: 1, windowBase: 0, windowEnd: 1,
        payload: new Uint8Array(1200),
    }
    const message = encodeSymbolMessageV2(repair)
    assert.equal(message.byteLength, 1221)
    assert.equal(parseSymbolMessage(message)?.payload.byteLength, 1200)
    assert.throws(() => encodeSymbolMessageV2({
        ...repair, payload: new Uint8Array(1201),
    }), /wire cap/)
})
test("selected v2 pipe rejects later v1 and malformed v2 packets without fallback", () => {
    const renderer = makeRenderer()
    const pipe = new FecDecodePipe(renderer, undefined, { now: () => 0 })
    assert.equal(pipe.configure({ version: 2, epoch: 7 }), true)
    const v2 = encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 7, seq: 0,
        payload: new Uint8Array(chunkFrameV2(0, true, 0, new Uint8Array([7]))[0]),
    })
    pipe.submitPacket(v2)
    assert.equal(renderer.units.length, 1, "configured v2 accepts matching epoch")
    pipe.submitPacket(sourceMsg(1, buildChunkPayload(1, 0, 1, 1, 1, new Uint8Array([1]))))
    const malformed = new Uint8Array(v2)
    malformed[malformed.length - 1] ^= 0xff
    pipe.submitPacket(malformed.buffer)
    pipe.submitPacket(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 8, seq: 1,
        payload: new Uint8Array(chunkFrameV2(1, true, 1, new Uint8Array([8]))[0]),
    }))
    assert.equal(renderer.units.length, 1, "v1, malformed v2, and wrong-epoch v2 leave the configured pipe unchanged")
    assert.equal(pipe.getStats().sourceSymbolsReceived, 1, "rejected packets do not mutate decoder state")
    pipe.dispose()
})

test("v2 chunks bind exact frame length, fragment cap, and CRC metadata", () => {
    const frame = new Uint8Array([1, 2, 3, 4])
    const [chunk] = chunkFrameV2(0x01020304, true, 0x05060708, frame)
    assert.deepEqual(parseChunkHeaderV2(chunk), {
        frameId: 0x01020304, chunkIndex: 0, chunkCount: 1, frameType: 1,
        timestampUs: 0x05060708, encodedFrameLen: 4, encodedFrameCrc32: 0xb63cfbcd,
    })
    const malformed = new Uint8Array(chunk)
    new DataView(malformed.buffer).setUint32(13, 5, true)
    assert.equal(parseChunkHeaderV2(malformed.buffer), null)
})
test("v2 chunk fragment and 4 MiB frame caps accept max and reject max plus one", () => {
    const fragment = new Uint8Array(1164)
    const [exactFragment] = chunkFrameV2(1, false, 0, fragment)
    assert.equal(new Uint8Array(exactFragment).byteLength, 1185)
    assert.equal(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 1, seq: 0, payload: new Uint8Array(exactFragment),
    }).byteLength, 1200)
    assert.equal(parseChunkHeaderV2(exactFragment)?.encodedFrameLen, 1164)

    const frame = new Uint8Array(4 * 1024 * 1024)
    const chunks = chunkFrameV2(2, false, 0, frame)
    assert.equal(chunks.length, Math.ceil((4 * 1024 * 1024) / 1164))
    assert.equal(parseChunkHeaderV2(chunks.at(-1))?.encodedFrameLen, 4 * 1024 * 1024)
    assert.throws(() => chunkFrameV2(3, false, 0, new Uint8Array(4 * 1024 * 1024 + 1)), /frame exceeds/)
})
test("v2 completed-frame reorder emits N before completed N+1", () => {
    let now = 0
    const renderer = makeRenderer()
    const discontinuities = []
    const pipe = new FecDecodePipe(renderer, undefined, {
        now: () => now,
        onDiscontinuity: event => discontinuities.push(event),
    })
    assert.equal(pipe.configure({ version: 2, epoch: 7 }), true)
    const [prime] = chunkFrameV2(9, true, 9, new Uint8Array([0x09]))
    const frame10 = new Uint8Array(1165).fill(0x10)
    const frame11 = new Uint8Array([0x11])
    const chunks10 = chunkFrameV2(10, false, 10, frame10)
    const [chunk11] = chunkFrameV2(11, false, 11, frame11)
    const send = (seq, payload) => pipe.submitPacket(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 7, seq, payload: new Uint8Array(payload),
    }))

    send(0, prime)
    send(1, chunks10[0])
    send(2, chunk11)
    assert.equal(renderer.units.length, 1, "N+1 waits for N after the initial key")
    send(3, chunks10[1])

    assert.deepEqual(renderer.units.slice(1).map(unit => unit.timestampMicroseconds), [10, 11])
    assert.deepEqual(discontinuities, [])
    pipe.dispose()
})

test("v2 reorder gap expires into a typed discontinuity and gates deltas", () => {
    let now = 0
    const renderer = makeRenderer()
    const discontinuities = []
    const pipe = new FecDecodePipe(renderer, undefined, {
        now: () => now,
        onDiscontinuity: event => discontinuities.push(event),
    })
    assert.equal(pipe.configure({ version: 2, epoch: 9 }), true)
    const [prime] = chunkFrameV2(19, true, 19, new Uint8Array([0x19]))
    const first = chunkFrameV2(20, false, 20, new Uint8Array(1165).fill(0x20))
    const [next] = chunkFrameV2(21, false, 21, new Uint8Array([0x21]))
    const send = (seq, payload) => pipe.submitPacket(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 9, seq, payload: new Uint8Array(payload),
    }))

    send(0, prime)
    send(1, first[0])
    send(2, next)
    now = 100
    pipe.tickTimer()

    assert.deepEqual(discontinuities, [{ epoch: 9, reason: "reorder-gap" }])
    assert.equal(pipe.pollRequestIdr(), true)
    pipe.dispose()
})
test("v2 completed reorder retains four records and evicts on the fifth", () => {
    let now = 0
    const renderer = makeRenderer()
    const discontinuities = []
    const pipe = new FecDecodePipe(renderer, undefined, {
        now: () => now,
        onDiscontinuity: event => discontinuities.push(event),
    })
    assert.equal(pipe.configure({ version: 2, epoch: 1 }), true)
    const send = (seq, frameId) => pipe.submitPacket(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 1, seq,
        payload: new Uint8Array(chunkFrameV2(frameId, true, frameId, new Uint8Array([frameId]))[0]),
    }))
    send(0, 0)
    for (let frameId = 2; frameId <= 5; frameId++) send(frameId, frameId)
    assert.equal(renderer.units.length, 1, "four completed records wait for missing frame 1")
    assert.deepEqual(discontinuities, [])
    send(6, 6)
    assert.deepEqual(discontinuities, [{ epoch: 1, reason: "reorder-gap" }])
    assert.equal(pipe.pollRequestIdr(), true)
    pipe.dispose()
})
test("v2 reassembly and reorder aggregate accepts 16 MiB and rejects cap plus one", () => {
    let now = 0
    const renderer = makeRenderer()
    const discontinuities = []
    const pipe = new FecDecodePipe(renderer, undefined, {
        now: () => now,
        onDiscontinuity: event => discontinuities.push(event),
    })
    assert.equal(pipe.configure({ version: 2, epoch: 1 }), true)
    let seq = 0
    const sendFrame = frameId => {
        const chunks = chunkFrameV2(frameId, true, frameId, new Uint8Array(FRAME_MAX_BYTES))
        for (const chunk of chunks) {
            pipe.submitPacket(encodeSymbolMessageV2({
                kind: "source", version: 2, epoch: 1, seq: seq++,
                payload: new Uint8Array(chunk),
            }))
        }
    }
    sendFrame(0)
    for (const frameId of [2, 3, 4, 5]) sendFrame(frameId)
    assert.equal(renderer.units.length, 1, "four completed 4 MiB records remain buffered behind frame 1")
    assert.deepEqual(discontinuities, [], "exact 16 MiB aggregate is retained")
    const [oneByte] = chunkFrameV2(6, true, 6, new Uint8Array([6]))
    pipe.submitPacket(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 1, seq: seq++, payload: new Uint8Array(oneByte),
    }))
    assert.deepEqual(discontinuities, [{ epoch: 1, reason: "memory-cap" }])
    assert.equal(pipe.pollRequestIdr(), true)
    pipe.dispose()
})
test("v2 epoch, metadata/CRC, cap, and same-epoch IDR recovery vectors", () => {
    let now = 0
    const renderer = makeRenderer()
    const discontinuities = []
    const pipe = new FecDecodePipe(renderer, undefined, {
        now: () => now,
        onDiscontinuity: event => discontinuities.push(event),
    })
    assert.equal(pipe.configure({ version: 2, epoch: 7 }), true)
    const send = (epoch, seq, payload) => pipe.submitPacket(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch, seq, payload: new Uint8Array(payload),
    }))

    // Establish the initial decode reference, then induce a same-epoch gap.
    send(7, 0, chunkFrameV2(0, true, 0, new Uint8Array([0]))[0])
    const first = chunkFrameV2(1, false, 1, new Uint8Array(1165))[0]
    const next = chunkFrameV2(2, false, 2, new Uint8Array([1]))[0]
    send(7, 1, first)
    send(7, 2, next)
    now = 100
    pipe.tickTimer()
    send(7, 3, chunkFrameV2(3, false, 3, new Uint8Array([2]))[0])
    assert.equal(renderer.units.length, 1, "delta remains gated after discontinuity")
    send(7, 4, chunkFrameV2(4, true, 4, new Uint8Array([3]))[0])
    assert.equal(renderer.units.length, 2, "keyframe recovers same epoch")
    send(7, 5, chunkFrameV2(5, false, 5, new Uint8Array([4]))[0])
    assert.equal(renderer.units.length, 3, "following same-epoch delta emits after the keyframe")

    // Renegotiating the epoch resets state; packets alone can never transition it.
    const count = renderer.units.length
    assert.equal(pipe.configure({ version: 2, epoch: 8 }), true)
    assert.deepEqual(discontinuities.at(-1), { epoch: 8, reason: "epoch-transition" })
    send(8, 0, chunkFrameV2(0, true, 0, new Uint8Array([4]))[0])
    assert.equal(renderer.units.length, count + 1, "renegotiated epoch admits its matching keyframe")
    send(7, 4, chunkFrameV2(1, true, 1, new Uint8Array([5]))[0])
    assert.equal(renderer.units.length, count + 1, "delayed old epoch is ignored")
    assert.equal(compareU32Serial(1, 0xffffffff), 1, "RFC1982 admits epoch 1 as newer across wrap")
    assert.equal(compareU32Serial(0xffffffff, 1), -1, "RFC1982 retains the reverse ordering across wrap")
    assert.equal(compareU32Serial(0x80000001, 1), null, "RFC1982 leaves exact half-range unordered")
    assert.equal(compareU32Serial(1, 0x80000001), null, "RFC1982 half-range unorderedness is symmetric")

    // Mixed cross-chunk metadata and complete-frame CRC failures are typed.
    const mixed = chunkFrameV2(10, false, 10, new Uint8Array(1165).fill(9))
    send(8, 5, mixed[0])
    const badMetadata = new Uint8Array(mixed[1])
    badMetadata[8] = 1
    send(8, 6, badMetadata)
    assert.deepEqual(discontinuities.at(-1), { epoch: 8, reason: "metadata-mismatch" })
    const crcMismatch = chunkFrameV2(0, true, 0, new Uint8Array(1165).fill(8))
    send(8, 7, crcMismatch[0])
    const corrupt = new Uint8Array(crcMismatch[1])
    corrupt[corrupt.length - 1] ^= 1
    send(8, 8, corrupt)
    assert.deepEqual(discontinuities.at(-1), { epoch: 8, reason: "frame-crc" })

    // Eight retained records are allowed; the ninth triggers the bounded eviction.
    const cap = new FecDecodePipe(makeRenderer(), undefined, { now: () => now })
    assert.equal(cap.configure({ version: 2, epoch: 1 }), true)
    for (let id = 0; id < 8; id++) {
        cap.submitPacket(encodeSymbolMessageV2({
            kind: "source", version: 2, epoch: 1, seq: id,
            payload: new Uint8Array(chunkFrameV2(id, false, id, new Uint8Array(1165))[0]),
        }))
    }
    cap.submitPacket(encodeSymbolMessageV2({
        kind: "source", version: 2, epoch: 1, seq: 8,
        payload: new Uint8Array(chunkFrameV2(8, false, 8, new Uint8Array(1165))[0]),
    }))
    assert.equal(cap.pollRequestIdr(), true, "ninth retained record is evicted")
    cap.dispose()
    pipe.dispose()
})

// ── Test: resolveFecCapability — every branch of the negotiation contract ──

test("resolveFecCapability: legacy tuple (omitted version, omitted epoch) + pipe -> configure v1", () => {
    const result = resolveFecCapability({}, true)
    assert.deepEqual(result, { kind: "configure", config: { version: 1 } })
})

test("resolveFecCapability: explicit v1 + omitted epoch + pipe -> configure v1", () => {
    const result = resolveFecCapability({ selected_fec_protocol_version: 1 }, true)
    assert.deepEqual(result, { kind: "configure", config: { version: 1 } })
})

test("resolveFecCapability: legacy/missing tuple + no pipe -> fallback (never fatal)", () => {
    const result = resolveFecCapability({}, false)
    assert.deepEqual(result, { kind: "fallback", config: { version: 1 } })
})

test("resolveFecCapability: v1 selected but epoch also present -> contradictory, typed fatal regardless of pipe", () => {
    const withPipe = resolveFecCapability({ selected_fec_protocol_version: 1, fec_epoch: 7 }, true)
    assert.equal(withPipe.kind, "fatal")
    assert.match(withPipe.reason, /Invalid negotiated FEC capability tuple/)
    const withoutPipe = resolveFecCapability({ selected_fec_protocol_version: 1, fec_epoch: 7 }, false)
    assert.equal(withoutPipe.kind, "fatal")
})

test("resolveFecCapability: valid v2 tuple + pipe -> configure v2", () => {
    const result = resolveFecCapability({ selected_fec_protocol_version: 2, fec_epoch: 42 }, true)
    assert.deepEqual(result, { kind: "configure", config: { version: 2, epoch: 42 } })
})

test("resolveFecCapability: valid v2 tuple + no pipe -> typed fatal (must never silently fall back)", () => {
    const result = resolveFecCapability({ selected_fec_protocol_version: 2, fec_epoch: 42 }, false)
    assert.equal(result.kind, "fatal")
    assert.match(result.reason, /Host selected FEC v2 but no FEC channels\/pipe are available/)
})

test("resolveFecCapability: v2 selected with malformed epoch (0, negative, non-integer, over cap) -> typed fatal", () => {
    for (const badEpoch of [0, -1, 1.5, 0x1_0000_0000]) {
        const result = resolveFecCapability({ selected_fec_protocol_version: 2, fec_epoch: badEpoch }, true)
        assert.equal(result.kind, "fatal", `epoch=${badEpoch} must be fatal`)
    }
})

test("resolveFecCapability: v2 selected with epoch entirely missing -> typed fatal, not silent legacy", () => {
    const result = resolveFecCapability({ selected_fec_protocol_version: 2 }, true)
    assert.equal(result.kind, "fatal")
})

test("resolveFecCapability: unknown protocol version number -> typed fatal", () => {
    const result = resolveFecCapability({ selected_fec_protocol_version: 3, fec_epoch: 1 }, true)
    assert.equal(result.kind, "fatal")
})

// ── Test: handleFecDataMessage — ArrayBuffer/Blob normalization + gating ──

test("handleFecDataMessage: accepted ArrayBuffer submits once and fires the progress callback once", async () => {
    const calls = []
    let submitCalls = 0
    const accepted = await handleFecDataMessage(
        new ArrayBuffer(4),
        (buf) => { submitCalls++; assert.ok(buf instanceof ArrayBuffer); return true },
        () => calls.push("progress"),
    )
    assert.equal(accepted, true)
    assert.equal(submitCalls, 1, "submit invoked exactly once")
    assert.deepEqual(calls, ["progress"])
})

test("handleFecDataMessage: rejected ArrayBuffer submits once but never fires the progress callback", async () => {
    const calls = []
    let submitCalls = 0
    const accepted = await handleFecDataMessage(
        new ArrayBuffer(4),
        (buf) => { submitCalls++; return false },
        () => calls.push("progress"),
    )
    assert.equal(accepted, false)
    assert.equal(submitCalls, 1)
    assert.deepEqual(calls, [], "no heartbeat for a rejected/malformed packet")
})

test("handleFecDataMessage: accepted Blob normalizes to ArrayBuffer, submits once, fires progress once", async () => {
    const bytes = new Uint8Array([1, 2, 3, 4])
    const blob = new Blob([bytes])
    const calls = []
    let submitCalls = 0
    const accepted = await handleFecDataMessage(
        blob,
        (buf) => {
            submitCalls++
            assert.ok(buf instanceof ArrayBuffer, "Blob must be normalized to ArrayBuffer before submit")
            assert.deepEqual(new Uint8Array(buf), bytes)
            return true
        },
        () => calls.push("progress"),
    )
    assert.equal(accepted, true)
    assert.equal(submitCalls, 1)
    assert.deepEqual(calls, ["progress"])
})

test("handleFecDataMessage: rejected Blob normalizes but never fires the progress callback", async () => {
    const blob = new Blob([new Uint8Array([9, 9])])
    const calls = []
    let submitCalls = 0
    const accepted = await handleFecDataMessage(
        blob,
        (buf) => { submitCalls++; return false },
        () => calls.push("progress"),
    )
    assert.equal(accepted, false)
    assert.equal(submitCalls, 1)
    assert.deepEqual(calls, [])
})

test("handleFecDataMessage: unsupported payload type (string) never submits and never fires progress", async () => {
    let submitCalls = 0
    const calls = []
    const accepted = await handleFecDataMessage(
        "not a buffer or blob",
        (buf) => { submitCalls++; return true },
        () => calls.push("progress"),
    )
    assert.equal(accepted, false)
    assert.equal(submitCalls, 0, "submit must never be invoked for an unsupported payload shape")
    assert.deepEqual(calls, [])
})

// ── Test: FecDecodePipe.dispose()/cleanup() — timer teardown + idempotency ──

test("cleanup() clears the real ACK timer, is idempotent, and forwards to base.cleanup() exactly once", () => {
    const realSetInterval = global.setInterval
    const realClearInterval = global.clearInterval
    const activeTimers = new Set()
    let nextTimerId = 0
    global.setInterval = () => {
        nextTimerId += 1
        activeTimers.add(nextTimerId)
        return nextTimerId
    }
    global.clearInterval = (id) => {
        activeTimers.delete(id)
    }
    try {
        const renderer = makeRenderer()
        let baseCleanupCalls = 0
        renderer.cleanup = () => { baseCleanupCalls++ }

        const pipe = new FecDecodePipe(renderer)
        assert.equal(activeTimers.size, 1, "constructing without an injected clock installs the real ACK timer")

        pipe.cleanup()
        assert.equal(activeTimers.size, 0, "cleanup() clears the ACK timer")
        assert.equal(baseCleanupCalls, 1, "cleanup() forwards to base.cleanup() exactly once")

        pipe.cleanup()
        assert.equal(baseCleanupCalls, 1, "a second cleanup() call is a no-op (idempotent)")
        assert.equal(activeTimers.size, 0, "timer stays cleared after the redundant cleanup() call")
    } finally {
        global.setInterval = realSetInterval
        global.clearInterval = realClearInterval
    }
})

test("dispose() clears the timer without forwarding to base.cleanup()", () => {
    const realSetInterval = global.setInterval
    const realClearInterval = global.clearInterval
    const activeTimers = new Set()
    let nextTimerId = 0
    global.setInterval = () => {
        nextTimerId += 1
        activeTimers.add(nextTimerId)
        return nextTimerId
    }
    global.clearInterval = (id) => {
        activeTimers.delete(id)
    }
    try {
        const renderer = makeRenderer()
        let baseCleanupCalls = 0
        renderer.cleanup = () => { baseCleanupCalls++ }

        const pipe = new FecDecodePipe(renderer)
        assert.equal(activeTimers.size, 1)

        pipe.dispose()
        assert.equal(activeTimers.size, 0, "dispose() clears the ACK timer")
        assert.equal(baseCleanupCalls, 0, "dispose() alone never forwards to base.cleanup()")

        // dispose() itself is safe to call again (ackTimerId already null).
        pipe.dispose()
        assert.equal(activeTimers.size, 0)
    } finally {
        global.setInterval = realSetInterval
        global.clearInterval = realClearInterval
    }
})
