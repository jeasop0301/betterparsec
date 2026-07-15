import assert from "node:assert/strict"
import test from "node:test"

import { parseCursorMessage, CURSOR_POS_LEN } from "../dist/stream/cursor_wire.js"

// ── Shared byte pins with cursor_wire.rs (change both or neither) ──────────

test("POS shared byte pin: visible, x=1000, y=-2, 2560x1440", () => {
    const bytes = new Uint8Array([
        0x00, 0x01, // kind, visible
        0xE8, 0x03, 0x00, 0x00, // x = 1000
        0xFE, 0xFF, 0xFF, 0xFF, // y = -2
        0x00, 0x0A, // vw = 2560
        0xA0, 0x05, // vh = 1440
    ])
    assert.equal(bytes.length, CURSOR_POS_LEN)
    assert.deepEqual(parseCursorMessage(bytes.buffer), {
        kind: 'pos',
        visible: true,
        x: 1000,
        y: -2,
        vw: 2560,
        vh: 1440,
    })
})

test("POS hidden pin: visible=0, 1920x1080", () => {
    const bytes = new Uint8Array([0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x07, 0x38, 0x04])
    const msg = parseCursorMessage(bytes.buffer)
    assert.equal(msg?.visible, false)
    assert.equal(msg?.vw, 1920)
    assert.equal(msg?.vh, 1080)
})

// ── Malformed input ─────────────────────────────────────────────────────────

test("empty buffer returns null", () => {
    assert.equal(parseCursorMessage(new ArrayBuffer(0)), null)
})

test("unknown kind returns null", () => {
    assert.equal(parseCursorMessage(new Uint8Array([0xFF]).buffer), null)
})

test("truncated POS (13 of 14 bytes) returns null", () => {
    assert.equal(parseCursorMessage(new ArrayBuffer(13)), null)
})
