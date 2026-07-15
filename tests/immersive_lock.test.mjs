import assert from "node:assert/strict"
import test from "node:test"

import { wantsPointerLock } from "../dist/stream/immersive_lock.js"

// ── M4 immersive mode: pointer-lock-wanted resolution ──────────────────────

test("relative mode always wants the lock", () => {
    assert.equal(wantsPointerLock("relative", false), true)
    assert.equal(wantsPointerLock("relative", true), true)
})

test("auto mode wants the lock only when the host last reported locked", () => {
    assert.equal(wantsPointerLock("auto", true), true)
    assert.equal(wantsPointerLock("auto", false), false)
})

test("follow, localCursor and pointAndDrag never want the lock", () => {
    assert.equal(wantsPointerLock("follow", true), false)
    assert.equal(wantsPointerLock("localCursor", true), false)
    assert.equal(wantsPointerLock("pointAndDrag", true), false)
})
