import assert from "node:assert/strict"
import test from "node:test"

import { ClientCursorAuthority } from "../dist/stream/cursor_render.js"

// Mirror of client-transport/src/cursor.rs ClientCursorAuthority tests
// (change both or neither).

test("cursor authority defaults to host-baked with no double cursor", () => {
    const auth = new ClientCursorAuthority()
    assert.equal(auth.owner(), "host")
    assert.equal(auth.hostBaked(), true)
    assert.equal(auth.renderClient(), false)
    assert.notEqual(auth.hostBaked(), auth.renderClient())
})

test("cursor authority generation gates ownership", () => {
    const auth = new ClientCursorAuthority()
    assert.equal(auth.applyAuthority(5, "client"), true)
    assert.equal(auth.renderClient(), true)
    // Stale generation rejected, ownership unchanged.
    assert.equal(auth.applyAuthority(3, "host"), false)
    assert.equal(auth.renderClient(), true)
    // Strictly newer takes over.
    assert.equal(auth.applyAuthority(6, "host"), true)
    assert.equal(auth.hostBaked(), true)
    assert.notEqual(auth.hostBaked(), auth.renderClient())
})

test("cursor authority dedups shape and gates on owner", () => {
    const auth = new ClientCursorAuthority()
    auth.noteShape(7)
    assert.equal(auth.takeShapeChange(), null) // host-baked: no client render
    auth.applyAuthority(1, "client")
    assert.equal(auth.takeShapeChange(), 7)
    assert.equal(auth.takeShapeChange(), null) // dedup
    auth.noteShape(8)
    assert.equal(auth.takeShapeChange(), 8)
    assert.equal(auth.takeShapeChange(), null)
    auth.noteShape(0)
    assert.equal(auth.takeShapeChange(), null) // none
})

test("cursor authority reconnect reverts to host and restores shape", () => {
    const auth = new ClientCursorAuthority()
    auth.applyAuthority(2, "client")
    auth.noteShape(42)
    assert.equal(auth.takeShapeChange(), 42)
    auth.onReconnect()
    assert.equal(auth.hostBaked(), true)
    assert.equal(auth.takeShapeChange(), null)
    // Re-claim (a fresh lower generation is fine after release) restores once.
    assert.equal(auth.applyAuthority(1, "client"), true)
    assert.equal(auth.takeShapeChange(), 42)
    assert.equal(auth.takeShapeChange(), null)
})
