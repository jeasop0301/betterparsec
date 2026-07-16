import assert from "node:assert/strict"
import test from "node:test"

import { knobsFor, canonicalVectors, DEFAULT_USER_TRADEOFFS } from "../dist/stream/mode.js"

// ── S1c: TS mirror of transport-core/src/mode.rs — parity source is the ──
// ── Rust `canonical_vectors()` table (hardcoded here as the shared truth) ──

const RUST_CANONICAL = {
    fast: {
        codecPref: "h264",
        width: 1280,
        height: 720,
        bitrateKbps: 5_000,
        fps: 60,
        audioExclusiveDefault: true,
    },
    medium: {
        codecPref: "hevc",
        width: 1920,
        height: 1080,
        bitrateKbps: 8_000,
        fps: 60,
        audioExclusiveDefault: true,
    },
    quality: {
        codecPref: "av1",
        width: 3840,
        height: 2160,
        bitrateKbps: 25_000,
        fps: 60,
        audioExclusiveDefault: false,
    },
}

test("canonicalVectors covers all modes in mode order", () => {
    const vectors = canonicalVectors()
    assert.equal(vectors.length, 3)
    assert.deepEqual(vectors.map(([mode]) => mode), ["fast", "medium", "quality"])
})

test("canonicalVectors matches the Rust canonical_vectors() table exactly", () => {
    for (const [mode, knobs] of canonicalVectors()) {
        assert.deepEqual(knobs, RUST_CANONICAL[mode], `mode ${mode} mismatch`)
    }
})

test("canonicalVectors matches knobsFor with zeroed UserTradeoffs", () => {
    for (const [mode, knobs] of canonicalVectors()) {
        assert.deepEqual(knobs, knobsFor(mode, { ...DEFAULT_USER_TRADEOFFS }))
    }
})

test("fast minimizes latency knobs", () => {
    const knobs = knobsFor("fast", { ...DEFAULT_USER_TRADEOFFS })
    assert.equal(knobs.codecPref, "h264")
    assert.ok(knobs.bitrateKbps < RUST_CANONICAL.medium.bitrateKbps)
    assert.ok(knobs.width <= RUST_CANONICAL.medium.width)
    assert.ok(knobs.height <= RUST_CANONICAL.medium.height)
    assert.ok(knobs.audioExclusiveDefault)
})

test("quality prefers av1 codec and maximizes bitrate/resolution", () => {
    const knobs = knobsFor("quality", { ...DEFAULT_USER_TRADEOFFS })
    assert.equal(knobs.codecPref, "av1")
    assert.ok(knobs.bitrateKbps > RUST_CANONICAL.medium.bitrateKbps)
    assert.ok(knobs.width >= RUST_CANONICAL.medium.width)
    assert.ok(knobs.height >= RUST_CANONICAL.medium.height)
})

test("medium sits between fast and quality", () => {
    const fast = RUST_CANONICAL.fast
    const medium = RUST_CANONICAL.medium
    const quality = RUST_CANONICAL.quality

    assert.ok(fast.bitrateKbps < medium.bitrateKbps)
    assert.ok(medium.bitrateKbps < quality.bitrateKbps)
    assert.equal(medium.codecPref, "hevc")
    assert.ok(fast.width <= medium.width && medium.width <= quality.width)
    assert.ok(fast.height <= medium.height && medium.height <= quality.height)
})

test("a non-zero user bitrate overrides only bitrate; other fields stay mode-default", () => {
    const user = { ...DEFAULT_USER_TRADEOFFS, bitrateKbps: 12_345 }
    const knobs = knobsFor("fast", user)
    const base = RUST_CANONICAL.fast

    assert.equal(knobs.bitrateKbps, 12_345)
    assert.equal(knobs.width, base.width)
    assert.equal(knobs.height, base.height)
    assert.equal(knobs.fps, base.fps)
    assert.equal(knobs.codecPref, base.codecPref)
})

test("each non-zero user field overrides only that single field", () => {
    const base = RUST_CANONICAL.quality

    const widthOverride = knobsFor("quality", { ...DEFAULT_USER_TRADEOFFS, width: 2560 })
    assert.equal(widthOverride.width, 2560)
    assert.equal(widthOverride.height, base.height)
    assert.equal(widthOverride.bitrateKbps, base.bitrateKbps)
    assert.equal(widthOverride.fps, base.fps)

    const heightOverride = knobsFor("quality", { ...DEFAULT_USER_TRADEOFFS, height: 1440 })
    assert.equal(heightOverride.height, 1440)
    assert.equal(heightOverride.width, base.width)

    const fpsOverride = knobsFor("quality", { ...DEFAULT_USER_TRADEOFFS, fps: 30 })
    assert.equal(fpsOverride.fps, 30)
    assert.equal(fpsOverride.bitrateKbps, base.bitrateKbps)
})

test("zeroed UserTradeoffs override nothing (all-default passthrough)", () => {
    const knobs = knobsFor("medium", { ...DEFAULT_USER_TRADEOFFS })
    assert.deepEqual(knobs, RUST_CANONICAL.medium)
})

test("ModeKnobs carries no free-lunch lever fields (pacing / fec_bias / aq)", () => {
    const knobs = knobsFor("medium", { ...DEFAULT_USER_TRADEOFFS })
    const fields = Object.keys(knobs).sort()
    assert.deepEqual(fields, [
        "audioExclusiveDefault",
        "bitrateKbps",
        "codecPref",
        "fps",
        "height",
        "width",
    ])
    assert.ok(!("pacing" in knobs))
    assert.ok(!("fecBias" in knobs))
    assert.ok(!("fec_bias" in knobs))
    assert.ok(!("aq" in knobs))
    assert.ok(!("spatialAq" in knobs))
    assert.ok(!("preset" in knobs))
    assert.ok(!("weightedPred" in knobs))
})
