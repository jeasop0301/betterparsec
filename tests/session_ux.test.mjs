import assert from "node:assert/strict"
import test from "node:test"

import { StallWatchdog, DEFAULT_WATCHDOG_CONFIG } from "../dist/stream/session_ux.js"

// Small config so tests read in round numbers.
const CFG = {
    stallIndicatorMs: 1000,
    idrFirstMs: 2000,
    idrRetryMs: 2000,
    idrMaxAttempts: 3,
    iceRestartMs: 10000,
    reconnectMs: 20000,
}

function makeDog() {
    const dog = new StallWatchdog(CFG)
    dog.start(0)
    return dog
}

/** Drive ticks every stepMs through toMs, collecting all actions. */
function drive(dog, toMs, stepMs = 250, fromMs = 0) {
    const actions = []
    for (let t = fromMs + stepMs; t <= toMs; t += stepMs) {
        actions.push(...dog.tick(t))
    }
    return actions
}

// ── Quiet stream ──────────────────────────────────────────────────────────

test("frames flowing: no actions, not stalled", () => {
    const dog = makeDog()
    for (let t = 16; t <= 5000; t += 16) {
        assert.deepEqual(dog.frameReceived(t), [])
        assert.deepEqual(dog.tick(t), [])
    }
    assert.equal(dog.stalled, false)
})

test("not running: tick and frameReceived are silent", () => {
    const dog = new StallWatchdog(CFG)
    assert.deepEqual(dog.tick(99999), [])
    assert.deepEqual(dog.frameReceived(99999), [])
})

// ── Ladder escalation ─────────────────────────────────────────────────────

test("full ladder fires in order with 250ms ticks", () => {
    const dog = makeDog()
    const actions = drive(dog, 21000)
    assert.deepEqual(actions.map((a) => a.type), [
        "stall",
        "requestIdr", "requestIdr", "requestIdr",
        "restartIce",
        "reconnect",
    ])
})

test("idr attempts are 1-based and spaced by idrRetryMs", () => {
    const dog = makeDog()
    const idrs = drive(dog, 9000).filter((a) => a.type === "requestIdr")
    assert.deepEqual(idrs.map((a) => a.attempt), [1, 2, 3])
})

test("indicator fires at stallIndicatorMs, not before", () => {
    const dog = makeDog()
    assert.deepEqual(dog.tick(999), [])
    assert.deepEqual(dog.tick(1000), [{ type: "stall" }])
    assert.equal(dog.stalled, true)
})

test("after reconnect the machine is silent until restarted", () => {
    const dog = makeDog()
    drive(dog, 21000)
    assert.deepEqual(dog.tick(60000), [])
    dog.start(60000)
    assert.deepEqual(drive(dog, 61000, 250, 60000), [{ type: "stall" }])
})

// ── One rung per tick (background-tab throttling guard) ───────────────────

test("a huge tick gap escalates one rung, not the whole ladder", () => {
    const dog = makeDog()
    // Tab was throttled: first tick arrives way past every threshold.
    assert.deepEqual(dog.tick(30000), [{ type: "stall" }])
    // Next frame recovers instead of tearing the session down.
    const rec = dog.frameReceived(30100)
    assert.deepEqual(rec.map((a) => a.type), ["recovered"])
})

// ── Recovery ──────────────────────────────────────────────────────────────

test("frame during stall emits recovered with duration and resets the ladder", () => {
    const dog = makeDog()
    drive(dog, 2000) // stall + idr#1
    const actions = dog.frameReceived(2500)
    assert.equal(actions.length, 1)
    assert.equal(actions[0].type, "recovered")
    assert.equal(actions[0].stalledMs, 2500)
    assert.equal(dog.stalled, false)
    // Ladder restarts from scratch on the next episode.
    const next = drive(dog, 5000, 250, 2500)
    assert.deepEqual(next.map((a) => a.type), ["stall", "requestIdr"])
    assert.equal(next[1].attempt, 1)
})

test("recovered is not emitted when there was no visible stall", () => {
    const dog = makeDog()
    drive(dog, 750) // below indicator threshold
    assert.deepEqual(dog.frameReceived(800), [])
})

// ── Never-delivering stream ───────────────────────────────────────────────

test("stream that never delivers a frame escalates from start()", () => {
    const dog = new StallWatchdog(CFG)
    dog.start(5000)
    const actions = drive(dog, 26000, 250, 5000)
    assert.deepEqual(actions.map((a) => a.type), [
        "stall",
        "requestIdr", "requestIdr", "requestIdr",
        "restartIce",
        "reconnect",
    ])
})

// ── Pause / resume ────────────────────────────────────────────────────────

test("paused watchdog does not escalate; resume restarts the clock", () => {
    const dog = makeDog()
    dog.pause()
    assert.deepEqual(drive(dog, 30000), [])
    assert.equal(dog.stalledMs(30000), 0)
    dog.resume(30000)
    // Hidden phase proved nothing: indicator needs a fresh threshold.
    assert.deepEqual(dog.tick(30999), [])
    assert.deepEqual(dog.tick(31000), [{ type: "stall" }])
})

test("resume without pause is a no-op (clock not reset)", () => {
    const dog = makeDog()
    drive(dog, 1000) // stall shown at t=1000
    dog.resume(50000)
    // idr #1 due at 2000 — the clock was NOT pushed to 50000.
    assert.deepEqual(dog.tick(2000), [{ type: "requestIdr", attempt: 1 }])
})

// ── stop / readback ───────────────────────────────────────────────────────

test("stop silences the machine and clears the indicator", () => {
    const dog = makeDog()
    drive(dog, 1500)
    assert.equal(dog.stalled, true)
    dog.stop()
    assert.equal(dog.stalled, false)
    assert.deepEqual(dog.tick(10000), [])
    assert.equal(dog.stalledMs(10000), 0)
})

test("stalledMs tracks elapsed since last frame", () => {
    const dog = makeDog()
    dog.frameReceived(400)
    assert.equal(dog.stalledMs(1400), 1000)
})

// ── Defaults sanity ───────────────────────────────────────────────────────

test("default config escalates in documented order", () => {
    const c = DEFAULT_WATCHDOG_CONFIG
    assert.ok(c.stallIndicatorMs < c.idrFirstMs)
    assert.ok(c.idrFirstMs + (c.idrMaxAttempts - 1) * c.idrRetryMs < c.iceRestartMs)
    assert.ok(c.iceRestartMs < c.reconnectMs)
})
