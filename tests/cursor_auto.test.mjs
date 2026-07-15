import assert from "node:assert/strict"
import test from "node:test"

import { CursorAutoMode, DEFAULT_CURSOR_AUTO_CONFIG } from "../dist/stream/cursor_auto.js"

const H = 150 // hysteresis used by every test (matches the default)

function makeAuto() {
    return new CursorAutoMode({ hysteresisMs: H })
}

test("default config pins 150ms hysteresis", () => {
    assert.equal(DEFAULT_CURSOR_AUTO_CONFIG.hysteresisMs, 150)
})

test("starts unlocked; visible samples produce no actions", () => {
    const auto = makeAuto()
    assert.equal(auto.locked, false)
    assert.equal(auto.onVisibility(true, 0), null)
    assert.equal(auto.tick(1000), null)
    assert.equal(auto.locked, false)
})

test("hidden commits lock only after hysteresis", () => {
    const auto = makeAuto()
    assert.equal(auto.onVisibility(false, 0), null)
    assert.equal(auto.tick(H - 1), null)
    assert.deepEqual(auto.tick(H), { type: 'lock' })
    assert.equal(auto.locked, true)
    // Committed: no duplicate action on further ticks/samples.
    assert.equal(auto.tick(H + 100), null)
    assert.equal(auto.onVisibility(false, H + 200), null)
})

test("flicker inside the window never switches", () => {
    const auto = makeAuto()
    assert.equal(auto.onVisibility(false, 0), null)
    // Cursor came back at 100ms — pending clears.
    assert.equal(auto.onVisibility(true, 100), null)
    assert.equal(auto.tick(1000), null)
    assert.equal(auto.locked, false)
})

test("flicker restarts the hysteresis clock", () => {
    const auto = makeAuto()
    auto.onVisibility(false, 0)
    auto.onVisibility(true, 100) // clears
    auto.onVisibility(false, 120) // pending restarts at 120
    assert.equal(auto.tick(120 + H - 1), null)
    assert.deepEqual(auto.tick(120 + H), { type: 'lock' })
})

test("visible while locked unlocks after hysteresis", () => {
    const auto = makeAuto()
    auto.onVisibility(false, 0)
    auto.tick(H) // locked
    assert.equal(auto.onVisibility(true, 500), null)
    assert.deepEqual(auto.tick(500 + H), { type: 'unlock' })
    assert.equal(auto.locked, false)
})

test("commit can happen on a visibility sample too (no tick needed)", () => {
    const auto = makeAuto()
    auto.onVisibility(false, 0)
    // The host re-sends (e.g. movement while hidden) after the window.
    assert.deepEqual(auto.onVisibility(false, H), { type: 'lock' })
})

test("zero hysteresis commits immediately", () => {
    const auto = new CursorAutoMode({ hysteresisMs: 0 })
    assert.deepEqual(auto.onVisibility(false, 10), { type: 'lock' })
    assert.deepEqual(auto.onVisibility(true, 20), { type: 'unlock' })
})

test("full round trip emits exactly one action per transition", () => {
    const auto = makeAuto()
    const actions = []
    let t = 0
    const drive = (visible, until) => {
        actions.push(auto.onVisibility(visible, t))
        for (; t <= until; t += 50) {
            actions.push(auto.tick(t))
        }
    }
    drive(false, 400) // lock once
    drive(true, 800) // unlock once
    drive(false, 1200) // lock once
    const emitted = actions.filter(Boolean)
    assert.deepEqual(emitted, [{ type: 'lock' }, { type: 'unlock' }, { type: 'lock' }])
})
