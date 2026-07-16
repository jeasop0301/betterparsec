// G004 deterministic fault matrix for the TS `FecDecodePipe` — mirrors, at
// the production-web-client layer, the same seeded scenarios exercised at
// the raw-`FecDecoder` layer by `transport-core/src/bin/fec_rig.rs`
// (Bernoulli + Gilbert-Elliott + reorder + duplication fault models) and at
// the `RxCore` layer by `client-transport/tests/video_fault_matrix.rs`.
//
// Public-API-only note on `getAccounting()`: `FecDecodePipe.decoder` is a
// TypeScript `private` field, not a JS `#private` field, so it survives
// compilation as a plain (reflectable) property — `pipe.decoder.getAccounting()`
// below relies on that, not on any new API surface added for this test. If a
// future refactor switches it to real `#private` fields, this file will
// start failing to compile/run, which is the point: it deliberately couples
// this test's reach to the current (TS-private-only) encapsulation level.
import assert from "node:assert/strict"
import test from "node:test"

import { FecEncoder } from "../dist/stream/video/fec.js"
import {
    chunkFrame, chunkFrameV2, encodeSymbolMessage, encodeSymbolMessageV2,
} from "../dist/stream/video/fec_wire.js"
import { FecDecodePipe } from "../dist/stream/video/fec_decode_pipe.js"

// ── Deterministic PRNG (xorshift32 — independently seeded/implemented per
// language, same discipline as fec_rig.rs's SplitMix64 and
// video_fault_matrix.rs's SplitMix64: no shared code across languages, just
// a fixed-seed, reproducible generator). ─────────────────────────────────

class Rng {
    constructor(seed) {
        this.state = (seed >>> 0) || 0x9e3779b9
    }
    nextU32() {
        let x = this.state
        x ^= x << 13; x >>>= 0
        x ^= x >>> 17
        x ^= (x << 5) >>> 0; x >>>= 0
        this.state = x >>> 0
        return this.state
    }
    nextFloat() {
        return this.nextU32() / 0x1_0000_0000
    }
    nextBelow(n) {
        return n <= 0 ? 0 : this.nextU32() % n
    }
}

// ── Gilbert-Elliott burst-loss channel (documented parameters) ────────────

class GeChannel {
    /** @param {{pGood: number, pBad: number, meanBurst: number, steadyP: number}} model */
    constructor(model) {
        this.model = model
        this.inBad = false
    }
    drop(rng) {
        const { pGood, pBad, meanBurst, steadyP } = this.model
        const b2g = 1 / Math.max(meanBurst, 1)
        const g2b = (steadyP / Math.max(1 - steadyP, 1e-9)) * b2g
        if (this.inBad) {
            if (rng.nextFloat() < b2g) this.inBad = false
        } else if (rng.nextFloat() < g2b) {
            this.inBad = true
        }
        const p = this.inBad ? pBad : pGood
        return rng.nextFloat() < p
    }
}

// ── Wire-stream builders ────────────────────────────────────────────────────

function framePayload(frameIdx, len) {
    const out = new Uint8Array(len)
    for (let i = 0; i < len; i++) out[i] = (Math.imul(i, 31) + frameIdx * 7) & 0xFF
    return out
}

/** v1 wire stream (source + repair messages, emission order) for `frameCount`
 * frames (frame 0 is the only keyframe), each chunked into multiple chunks. */
function buildV1Stream(frameCount, ratioNum, ratioDen) {
    const enc = new FecEncoder({
        redundancyNumerator: ratioNum, redundancyDenominator: ratioDen,
        windowMaxSymbols: 64, windowMaxBytes: 1 << 20,
    })
    let seq = 0
    const wire = []
    for (let f = 0; f < frameCount; f++) {
        const data = framePayload(f, 3000)
        for (const chunk of chunkFrame(f, f === 0, (f + 1) * 16_667, data)) {
            const out = enc.pushSource(seq, new Uint8Array(chunk))
            wire.push(encodeSymbolMessage(out.source))
            for (const rep of out.repairs) wire.push(encodeSymbolMessage(rep))
            seq++
        }
    }
    return wire
}

/** v2 wire stream for `frameCount` frames under a fixed `epoch`. */
function buildV2Stream(epoch, frameCount, ratioNum, ratioDen) {
    const enc = new FecEncoder({
        redundancyNumerator: ratioNum, redundancyDenominator: ratioDen,
        windowMaxSymbols: 64, windowMaxBytes: 1 << 20,
    })
    let seq = 0
    const wire = []
    for (let f = 0; f < frameCount; f++) {
        const data = framePayload(f, 1600)
        for (const chunk of chunkFrameV2(f, f === 0, (f + 1) * 16_667, data)) {
            const out = enc.pushSource(seq, new Uint8Array(chunk))
            wire.push(encodeSymbolMessageV2({ ...out.source, version: 2, epoch }))
            for (const rep of out.repairs) wire.push(encodeSymbolMessageV2({ ...rep, version: 2, epoch }))
            seq++
        }
    }
    return wire
}

/** Applies loss (Bernoulli xor Gilbert-Elliott), then a deterministic
 * sliding-window reorder, then duplication, in that order — same discipline
 * as fec_rig.rs's/video_fault_matrix.rs's `apply_faults`. */
function applyFaults(wire, seed, bernoulliP, geModel, reorderWindow, dupP) {
    const rng = new Rng(seed)
    const ge = geModel ? new GeChannel(geModel) : null

    const survivors = []
    for (const msg of wire) {
        const dropped = ge ? ge.drop(rng) : rng.nextFloat() < bernoulliP
        if (!dropped) survivors.push(msg)
    }

    if (reorderWindow > 1) {
        for (let i = 0; i + reorderWindow <= survivors.length; i++) {
            const j = i + rng.nextBelow(reorderWindow)
            const tmp = survivors[i]; survivors[i] = survivors[j]; survivors[j] = tmp
        }
    }

    const out = []
    for (const msg of survivors) {
        out.push(msg)
        if (rng.nextFloat() < dupP) out.push(msg)
    }
    return out
}

// ── Harness: drives FecDecodePipe and enforces the cross-scenario invariant ─

/**
 * Wraps one `FecDecodePipe` with a capturing renderer and enforces, on every
 * scenario: zero delta units are ever observed while the pipe is gated
 * (`awaitingIdr`) — the pipe drops gated deltas internally (never calls
 * `submitDecodeUnit`), so the harness's job is to prove the *first* unit
 * delivered after any discontinuity is a key frame, not merely that no delta
 * appeared (that part is structural/always true by construction).
 */
class Harness {
    constructor(options = {}) {
        this.units = []
        this.discontinuities = []
        this.gated = false
        const renderer = {
            implementationName: "fault-matrix-renderer",
            submitDecodeUnit: (unit) => this.onUnit(unit),
            getBase: () => null,
        }
        this.pipe = new FecDecodePipe(renderer, undefined, {
            now: () => 0, // fixed clock: no wall-clock dependence
            onDiscontinuity: (d) => this.onDiscontinuity(d),
            ...options,
        })
    }

    onDiscontinuity(d) {
        this.discontinuities.push(d)
        this.gated = true
    }

    onUnit(unit) {
        if (this.gated) {
            assert.equal(unit.type, "key", "delta frame delivered while gated after a discontinuity")
            this.gated = false
        }
        this.units.push(unit)
    }

    feed(msgs) {
        for (const m of msgs) this.pipe.submitPacket(m)
    }
}

function runBounded(fn) {
    const start = Date.now()
    fn()
    const elapsed = Date.now() - start
    assert.ok(elapsed < 5000, `scenario exceeded its 5000ms runtime budget (took ${elapsed}ms)`)
}

// ── Cell: clean stream ──────────────────────────────────────────────────────

test("clean stream: all frames deliver in order, zero discontinuities", () => {
    runBounded(() => {
        const wire = buildV1Stream(12, 1, 4)
        const h = new Harness()
        h.feed(wire)

        assert.equal(h.discontinuities.length, 0, "a lossless stream must never discontinue")
        assert.equal(h.units.length, 12)
        h.units.forEach((unit, i) => {
            assert.equal(unit.timestampMicroseconds, (i + 1) * 16_667)
            assert.deepEqual(new Uint8Array(unit.data), framePayload(i, 3000))
        })
    })
})

// ── Cell: reorder ────────────────────────────────────────────────────────────

test("reorder within window: gates then still makes forward progress", () => {
    runBounded(() => {
        // NOTE (finding, not a test-authoring bug): mirrors the Rust decoder's
        // contract exactly (see video_fault_matrix.rs's reorder cell) —
        // `FecDecoder.advanceContiguous` treats a gap *bounded* by a
        // later-arriving seq as permanently unrecoverable, so sustained
        // reordering surfaces as a real discontinuity, not a transparent
        // recovery. This cell proves gate-then-progress, not "every frame
        // survives unscathed".
        const wire = buildV1Stream(10, 1, 2)
        const faulted = applyFaults(wire, 4, 0, null, 3, 0)
        const h = new Harness()
        h.feed(faulted)

        assert.ok(h.discontinuities.length > 0, "this seed must actually exercise the gate")
        assert.ok(h.units.length > 0, "reordering must not stall the stream permanently")
        let last = -1
        for (const unit of h.units) {
            const frameIdx = Math.round(unit.timestampMicroseconds / 16_667) - 1
            assert.ok(frameIdx > last, "frame order must strictly increase")
            assert.deepEqual(new Uint8Array(unit.data), framePayload(frameIdx, 3000))
            last = frameIdx
        }
    })
})

// ── Cell: random (Bernoulli) loss 0.1% / 1% / 2% / 5% ───────────────────────

function randomLossScenario(lossP, seed) {
    runBounded(() => {
        const wire = buildV1Stream(40, 1, 2)
        const faulted = applyFaults(wire, seed, lossP, null, 1, 0)
        const h = new Harness()
        h.feed(faulted)
        assert.equal(
            h.units.length, 40,
            `1/2 FEC ratio must recover every frame at ${lossP * 100}% independent loss`,
        )
    })
}

test("random loss 0.1%: recovers via FEC", () => { randomLossScenario(0.001, 0xF00D1001) })
test("random loss 1%: recovers via FEC", () => { randomLossScenario(0.01, 0xF00D1002) })
test("random loss 2%: recovers via FEC", () => { randomLossScenario(0.02, 0xF00D1003) })
test("random loss 5%: recovers via FEC", () => { randomLossScenario(0.05, 0xF00D1004) })

// ── Cell: Gilbert-Elliott burst loss 20% ────────────────────────────────────

test("Gilbert-Elliott burst loss 20%: recovers or gates cleanly", () => {
    runBounded(() => {
        const wire = buildV1Stream(60, 1, 2)
        const ge = { pGood: 0, pBad: 1, meanBurst: 4, steadyP: 0.20 }
        const faulted = applyFaults(wire, 0xF00D2001, 0, ge, 1, 0)
        const h = new Harness()
        h.feed(faulted)

        assert.ok(h.units.length > 0, "burst loss must not stall the stream permanently")
        let last = -1
        for (const unit of h.units) {
            const frameIdx = Math.round(unit.timestampMicroseconds / 16_667) - 1
            assert.ok(frameIdx > last, "frame ids must strictly increase")
            last = frameIdx
        }
    })
})

// ── Cell: malformed / truncated messages ────────────────────────────────────

test("malformed and truncated messages are silently ignored", () => {
    runBounded(() => {
        const h = new Harness()
        h.pipe.configure({ version: 1 })

        const garbage = [
            new ArrayBuffer(0),
            new Uint8Array([0xFF, 1, 2, 3]).buffer,
            new Uint8Array([0x00, 1, 2]).buffer, // truncated v1 source (needs 5-byte header)
            new Uint8Array([0x00, 0, 0, 0, 0, 1, 2]).buffer, // valid header, short chunk payload
        ]
        h.feed(garbage)
        assert.equal(h.units.length, 0, "malformed input must produce zero delivered units")
        assert.equal(h.discontinuities.length, 0, "malformed input must never itself discontinue")

        // The pipe must still be usable afterwards. Uses a fresh seq (100, not
        // 0): the last garbage message above is a *valid* FEC source symbol
        // (seq 0) whose chunk payload is merely too short to be a chunk
        // header, so the decoder legitimately consumes seq 0 — re-using seq 0
        // here would be silently rejected as a duplicate.
        const chunk = chunkFrame(0, true, 1000, new Uint8Array([7, 8, 9]))[0]
        const msg = encodeSymbolMessage({ kind: "source", seq: 100, payload: new Uint8Array(chunk) })
        h.feed([msg])
        assert.equal(h.units.length, 1)
        assert.deepEqual(new Uint8Array(h.units[0].data), new Uint8Array([7, 8, 9]))
    })
})

// ── Cell: CRC-corrupt v2 frame ──────────────────────────────────────────────

test("CRC-corrupt v2 frame triggers a typed discontinuity", () => {
    runBounded(() => {
        const h = new Harness()
        h.pipe.configure({ version: 2, epoch: 9 })

        const chunk = new Uint8Array(chunkFrameV2(0, true, 0, new Uint8Array([1, 2, 3]))[0])
        chunk[chunk.length - 1] ^= 1 // flip one payload byte -> CRC mismatch
        const msg = encodeSymbolMessageV2({ kind: "source", seq: 0, payload: chunk, version: 2, epoch: 9 })

        h.feed([msg])
        assert.ok(
            h.discontinuities.some((d) => d.reason === "frame-crc"),
            "corrupted encoded-frame CRC must surface as frame-crc, not silently drop",
        )
        assert.equal(h.units.length, 0, "the corrupt frame itself must never deliver")
    })
})

// ── Cell: queue overload (reassembly record-cap ordering) ───────────────────

test("record cap: reassembly-eviction discontinuity fires before any partial state survives", () => {
    runBounded(() => {
        const h = new Harness()
        h.pipe.configure({ version: 1 })

        // 8 distinct never-completing pending frames (first chunk only, of a
        // multi-chunk frame) do not yet trip the cap.
        for (let i = 0; i < 8; i++) {
            const chunks = chunkFrame(i, false, i * 1000, new Uint8Array(4000).fill(i))
            const msg = encodeSymbolMessage({ kind: "source", seq: i, payload: new Uint8Array(chunks[0]) })
            h.feed([msg])
        }
        assert.equal(h.discontinuities.length, 0, "exactly 8 pending frames must not yet trip the cap")

        // The 9th distinct pending frame trips MAX_PENDING_FRAMES (8).
        const ninthChunks = chunkFrame(8, false, 8000, new Uint8Array(4000).fill(8))
        const ninthMsg = encodeSymbolMessage({ kind: "source", seq: 8, payload: new Uint8Array(ninthChunks[0]) })
        h.feed([ninthMsg])
        assert.ok(
            h.discontinuities.some((d) => d.reason === "reassembly-eviction"),
            "the 9th distinct pending frame must trip the reassembly cap",
        )
    })
})

// ── Cell: epoch transition (in-place, same pipe instance) ───────────────────

test("epoch transition mid-stream resets the decoder and keeps delivering", () => {
    runBounded(() => {
        const h = new Harness()
        assert.ok(h.pipe.configure({ version: 2, epoch: 7 }))
        const first = buildV2Stream(7, 3, 0, 1)
        h.feed(first)
        assert.equal(h.units.length, 3)

        // A different epoch on the *same* pipe instance is a real in-place
        // transition: `FecDecodePipe.configure`'s `changed` branch resets the
        // decoder/reassembly state and fires an epoch-transition discontinuity.
        assert.ok(h.pipe.configure({ version: 2, epoch: 8 }))
        assert.ok(
            h.discontinuities.some((d) => d.reason === "epoch-transition"),
            "switching epoch on the same pipe must fire epoch-transition",
        )

        const second = buildV2Stream(8, 3, 0, 1)
        h.feed(second)
        assert.equal(h.units.length, 3 + 3)
    })
})

// ── Cell: same-epoch "renegotiation" (fresh instance — see note) ───────────

test("same-epoch renegotiation is modeled as a fresh pipe instance", () => {
    runBounded(() => {
        // NOTE (cross-language contract asymmetry, not a test gap):
        // `FecDecodePipe.configure()` treats a repeat call with the *same*
        // version+epoch as a no-op (`changed` is false, `firstExplicitConfig`
        // is false) — unlike Rust's `RxCore::begin_fec_negotiation()` +
        // `configure_fec()`, which always tears down and rearms a fresh
        // generation even for the same epoch. A same-epoch "reconnect" at
        // this layer is therefore only observable as a fresh `FecDecodePipe`
        // instance, matching how the production web client actually
        // reconnects (a new pipe per session), not a call on the old one.
        const first = new Harness()
        assert.ok(first.pipe.configure({ version: 2, epoch: 7 }))
        first.feed(buildV2Stream(7, 3, 0, 1))
        assert.equal(first.units.length, 3)

        const second = new Harness()
        assert.equal(second.units.length, 0, "a fresh instance carries no state from the prior one")
        assert.equal(second.discontinuities.length, 0)
        assert.ok(second.pipe.configure({ version: 2, epoch: 7 }))
        second.feed(buildV2Stream(7, 3, 0, 1))
        assert.equal(second.units.length, 3)
        assert.deepEqual(
            new Uint8Array(second.units[0].data),
            new Uint8Array(first.units[0].data),
            "the new generation reproduces byte-identical frame 0 content",
        )
    })
})

// ── Cell: failed-key (never acked) ──────────────────────────────────────────

test("failed key (never arrives): deltas stay gated forever", () => {
    runBounded(() => {
        const h = new Harness()
        h.pipe.configure({ version: 1 })

        // 9 distinct never-completing pending frames trips reassembly-eviction
        // (matches the record-cap cell) without ever completing a keyframe.
        for (let i = 0; i < 9; i++) {
            const chunks = chunkFrame(i, false, i * 1000, new Uint8Array(4000).fill(i))
            const msg = encodeSymbolMessage({ kind: "source", seq: i, payload: new Uint8Array(chunks[0]) })
            h.feed([msg])
        }
        assert.ok(h.discontinuities.some((d) => d.reason === "reassembly-eviction"))
        assert.ok(h.pipe.pollRequestIdr(), "the reset must request an IDR")
        assert.equal(h.units.length, 0)

        // Now feed only delta frames, forever (bounded to a handful here):
        // none may ever be delivered — there is no key frame to clear the gate.
        for (let i = 100; i < 120; i++) {
            const chunk = chunkFrame(i, false, i * 1000, new Uint8Array([i & 0xFF]))[0]
            const msg = encodeSymbolMessage({ kind: "source", seq: 9 + (i - 100), payload: new Uint8Array(chunk) })
            h.feed([msg])
        }
        assert.equal(h.units.length, 0, "a delta frame must never surface while the gate stays open")
        assert.ok(
            h.pipe.getStats().framesDroppedAwaitingIdr >= 20,
            "every gated delta must count against framesDroppedAwaitingIdr",
        )
    })
})

// ── Cell: 100 fresh pipe instances carry no orphan state ────────────────────

test("100 fresh pipe instances carry no orphan state", () => {
    runBounded(() => {
        for (let generation = 0; generation < 100; generation++) {
            const h = new Harness()
            assert.equal(h.units.length, 0, `gen ${generation}: fresh pipe delivered nothing yet`)
            assert.equal(h.discontinuities.length, 0, `gen ${generation}: fresh pipe has no discontinuities`)
            assert.deepEqual(
                h.pipe.getStats(),
                {
                    sourceSymbolsReceived: 0, repairSymbolsReceived: 0, symbolsRecovered: 0,
                    lossSpans: 0, lossSpansRecovered: 0, framesRecovered: 0, framesDroppedAwaitingIdr: 0,
                },
                `gen ${generation}: fresh pipe's stats are zeroed`,
            )

            const wire = buildV1Stream(4, 1, 2)
            const faulted = applyFaults(wire, 0xF00D3000 + generation, 0.02, null, 3, 0.05)
            h.feed(faulted)
            assert.ok(
                h.units.length <= 4,
                `gen ${generation}: must never observe more frames than were encoded`,
            )
        }
    })
})

// ── Cell: FEC retained bytes never exceed 16 MiB (via getAccounting) ───────

test("FecDecoder.getAccounting().retainedBytes never exceeds the pipe's 16 MiB cap", () => {
    runBounded(() => {
        const h = new Harness()
        h.pipe.configure({ version: 1 })

        const wire = buildV1Stream(30, 1, 2)
        const faulted = applyFaults(wire, 0xF00D4001, 0.03, null, 1, 0.02)

        let peakRetainedBytes = 0
        for (const msg of faulted) {
            h.pipe.submitPacket(msg)
            // `pipe.decoder` is a TS-private (JS-public) field — see the
            // file-level note at the top of this test.
            const accounting = h.pipe.decoder.getAccounting()
            assert.ok(
                accounting.retainedBytes <= 16 * 1024 * 1024,
                `retainedBytes ${accounting.retainedBytes} exceeded the 16 MiB cap`,
            )
            peakRetainedBytes = Math.max(peakRetainedBytes, accounting.retainedBytes)
        }
        assert.ok(peakRetainedBytes > 0, "the scenario must actually retain some decoder state")
    })
})
