import assert from "node:assert/strict"
import test from "node:test"

import { ContactTracker, normalizePen, transformContact, MAX_CONTACTS, PEN_PRESSURE_MAX } from "../dist/stream/pen_touch.js"

// Mirror of common/src/pen_touch.rs tests.

test("contact lifetime invariants", () => {
    const t = new ContactTracker()
    assert.equal(t.apply(1, "move", 0, 0), "rejected")
    assert.equal(t.apply(1, "up", 0, 0), "rejected")
    assert.equal(t.apply(1, "down", 10, 20), "started")
    assert.equal(t.isDown(1), true)
    assert.equal(t.apply(1, "down", 0, 0), "rejected") // duplicate
    assert.equal(t.apply(1, "move", 30, 40), "updated")
    assert.equal(t.apply(1, "up", 30, 40), "ended")
    assert.equal(t.isDown(1), false)
    assert.equal(t.apply(1, "move", 0, 0), "rejected")
})

test("multiple contacts tracked and bounded", () => {
    const t = new ContactTracker()
    for (let id = 0; id < MAX_CONTACTS; id++) assert.equal(t.apply(id, "down", 0, 0), "started")
    assert.equal(t.activeCount(), MAX_CONTACTS)
    assert.equal(t.apply(999, "down", 0, 0), "rejected")
    assert.equal(t.apply(0, "up", 0, 0), "ended")
    assert.equal(t.activeCount(), MAX_CONTACTS - 1)
})

test("cancelAll clears every contact", () => {
    const t = new ContactTracker()
    t.apply(1, "down", 0, 0)
    t.apply(2, "down", 0, 0)
    assert.equal(t.cancelAll(), 2)
    assert.equal(t.activeCount(), 0)
    assert.equal(t.apply(1, "move", 0, 0), "rejected")
})

test("pen pressure and tilt are bounded", () => {
    assert.deepEqual(normalizePen(2000, 200, -200), [PEN_PRESSURE_MAX, 90, -90])
    assert.deepEqual(normalizePen(512, 45, -30), [512, 45, -30])
    assert.deepEqual(normalizePen(0, 0, 0), [0, 0, 0])
})

test("contact display transform", () => {
    assert.deepEqual(transformContact(960, 540, 1920, 1080, 3840, 2160), [1920, 1080])
    assert.equal(transformContact(0, 0, 0, 1080, 3840, 2160), null)
})
