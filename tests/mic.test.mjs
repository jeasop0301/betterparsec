import assert from "node:assert/strict"
import test from "node:test"

import { MicrophoneGate } from "../dist/stream/mic.js"

// Mirror of common/src/mic.rs tests.

function ready() {
    const g = new MicrophoneGate()
    g.setPermission("granted")
    g.setDevice(1)
    g.setSessionActive(true)
    return g
}

test("default gate never captures", () => {
    const g = new MicrophoneGate()
    assert.equal(g.canCapture(), false)
    assert.equal(g.permission(), "prompt")
})

test("all conditions required to capture", () => {
    assert.equal(ready().canCapture(), true)

    let g = ready()
    g.setPermission("denied")
    assert.equal(g.canCapture(), false)

    g = ready()
    g.setDevice(null)
    assert.equal(g.canCapture(), false)

    g = ready()
    g.setSessionActive(false)
    assert.equal(g.canCapture(), false)
})

test("mute and fail-closed stop capture", () => {
    const g = ready()
    g.mute()
    assert.equal(g.isMuted(), true)
    assert.equal(g.canCapture(), false)
    g.unmute()
    assert.equal(g.canCapture(), true)
    g.failClosed()
    assert.equal(g.canCapture(), false)
})

test("device switch drops capture until confirmed", () => {
    const g = ready()
    g.setDevice(null)
    assert.equal(g.canCapture(), false)
    g.setDevice(2)
    assert.equal(g.canCapture(), true)
})

test("reconnect fails closed and requires re-establishment", () => {
    const g = ready()
    g.onReconnect()
    assert.equal(g.canCapture(), false)
    assert.equal(g.permission(), "granted")
    g.setSessionActive(true)
    assert.equal(g.canCapture(), false) // device still missing
    g.setDevice(3)
    assert.equal(g.canCapture(), true)
})
