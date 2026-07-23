import assert from "node:assert/strict"
import test from "node:test"

import { channelCount, opusOrder, negotiate } from "../dist/stream/surround.js"

// Mirror of common/src/surround.rs tests.

test("channel counts", () => {
    assert.equal(channelCount("mono"), 1)
    assert.equal(channelCount("stereo"), 2)
    assert.equal(channelCount("surround51"), 6)
    assert.equal(channelCount("surround71"), 8)
})

test("opus channel-identity vectors", () => {
    assert.deepEqual(opusOrder("stereo"), ["front_left", "front_right"])
    assert.deepEqual(opusOrder("surround51"), ["front_left", "front_center", "front_right", "back_left", "back_right", "lfe"])
    assert.deepEqual(opusOrder("surround71"), ["front_left", "front_center", "front_right", "side_left", "side_right", "back_left", "back_right", "lfe"])
    for (const l of ["mono", "stereo", "surround51", "surround71"]) {
        assert.equal(opusOrder(l).length, channelCount(l))
    }
})

test("negotiation selects the largest mutually-supported layout", () => {
    assert.deepEqual(negotiate("surround71", 8), { selected: "surround71", downgraded: false })
    assert.deepEqual(negotiate("surround71", 2), { selected: "stereo", downgraded: true })
    assert.deepEqual(negotiate("surround71", 6), { selected: "surround51", downgraded: true })
    assert.deepEqual(negotiate("surround51", 8), { selected: "surround51", downgraded: false })
    assert.deepEqual(negotiate("stereo", 8), { selected: "stereo", downgraded: false })
    assert.deepEqual(negotiate("surround51", 1), { selected: "mono", downgraded: true })
})
