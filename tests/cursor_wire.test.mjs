import assert from "node:assert/strict"
import test from "node:test"

import { parseCursorMessage, CURSOR_POS_LEN, CURSOR_SHAPE_HEADER_LEN, CURSOR_SHAPE_MAX_PNG_LEN } from "../dist/stream/cursor_wire.js"

// ── Shared byte pins with cursor_wire.rs (change both or neither) ──────────

test("POS shared byte pin: visible, x=1000, y=-2, 2560x1440, shape_id=7", () => {
    const bytes = new Uint8Array([
        0x00, 0x01, // kind, visible
        0xE8, 0x03, 0x00, 0x00, // x = 1000
        0xFE, 0xFF, 0xFF, 0xFF, // y = -2
        0x00, 0x0A, // vw = 2560
        0xA0, 0x05, // vh = 1440
        0x07, 0x00, 0x00, 0x00, // shape_id = 7
    ])
    assert.equal(bytes.length, CURSOR_POS_LEN)
    assert.deepEqual(parseCursorMessage(bytes.buffer), {
        kind: 'pos',
        visible: true,
        x: 1000,
        y: -2,
        vw: 2560,
        vh: 1440,
        shapeId: 7,
    })
})

test("POS hidden pin: visible=0, 1920x1080, shape_id=0", () => {
    const bytes = new Uint8Array([
        0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x07, 0x38, 0x04,
        0x00, 0x00, 0x00, 0x00, // shape_id = 0
    ])
    assert.equal(bytes.length, CURSOR_POS_LEN)
    const msg = parseCursorMessage(bytes.buffer)
    assert.equal(msg?.visible, false)
    assert.equal(msg?.vw, 1920)
    assert.equal(msg?.vh, 1080)
    assert.equal(msg?.shapeId, 0)
})

// ── SHAPE ────────────────────────────────────────────────────────────────────

test("SHAPE shared byte pin: shape_id=7, 32x32, hotspot (3,4), 3-byte png", () => {
    const bytes = new Uint8Array([
        0x01, // kind = shape
        0x07, 0x00, 0x00, 0x00, // shape_id = 7
        0x20, 0x00, // w = 32
        0x20, 0x00, // h = 32
        0x03, 0x00, // hot_x = 3
        0x04, 0x00, // hot_y = 4
        0x03, 0x00, 0x00, 0x00, // png_len = 3
        0xAA, 0xBB, 0xCC, // png bytes
    ])
    assert.deepEqual(parseCursorMessage(bytes.buffer), {
        kind: 'shape',
        shapeId: 7,
        w: 32,
        h: 32,
        hotX: 3,
        hotY: 4,
        png: new Uint8Array([0xAA, 0xBB, 0xCC]),
    })
})

test("SHAPE roundtrip against a tiny fixed PNG byte array", () => {
    // Not a full valid PNG (irrelevant to the wire parser, which treats the
    // payload as opaque bytes) — a stable fixture standing in for "some PNG
    // bytes produced by the host".
    const png = new Uint8Array([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x11])
    const header = new Uint8Array([
        0x01,
        0x2A, 0x00, 0x00, 0x00, // shape_id = 42
        0x10, 0x00, // w = 16
        0x10, 0x00, // h = 16
        0x08, 0x00, // hot_x = 8
        0x08, 0x00, // hot_y = 8
        png.length, 0x00, 0x00, 0x00, // png_len
    ])
    const bytes = new Uint8Array(header.length + png.length)
    bytes.set(header, 0)
    bytes.set(png, header.length)

    const msg = parseCursorMessage(bytes.buffer)
    assert.equal(msg?.kind, 'shape')
    assert.equal(msg?.shapeId, 42)
    assert.equal(msg?.w, 16)
    assert.equal(msg?.h, 16)
    assert.equal(msg?.hotX, 8)
    assert.equal(msg?.hotY, 8)
    assert.deepEqual(msg?.png, png)
})

test("SHAPE refuses png_len over the cap even when the buffer has enough bytes", () => {
    const header = new DataView(new ArrayBuffer(CURSOR_SHAPE_HEADER_LEN))
    header.setUint8(0, 0x01)
    header.setUint32(1, 1, true) // shape_id
    header.setUint16(5, 1, true) // w
    header.setUint16(7, 1, true) // h
    header.setUint16(9, 0, true) // hot_x
    header.setUint16(11, 0, true) // hot_y
    header.setUint32(13, CURSOR_SHAPE_MAX_PNG_LEN + 1, true) // png_len over cap

    const bytes = new Uint8Array(CURSOR_SHAPE_HEADER_LEN + CURSOR_SHAPE_MAX_PNG_LEN + 1)
    bytes.set(new Uint8Array(header.buffer), 0)

    assert.equal(parseCursorMessage(bytes.buffer), null)
})

test("SHAPE truncated (header claims more png bytes than present) returns null", () => {
    const header = new DataView(new ArrayBuffer(CURSOR_SHAPE_HEADER_LEN))
    header.setUint8(0, 0x01)
    header.setUint32(1, 1, true)
    header.setUint16(5, 1, true)
    header.setUint16(7, 1, true)
    header.setUint16(9, 0, true)
    header.setUint16(11, 0, true)
    header.setUint32(13, 10, true) // claims 10 png bytes

    const bytes = new Uint8Array(CURSOR_SHAPE_HEADER_LEN + 2) // only 2 present
    bytes.set(new Uint8Array(header.buffer), 0)

    assert.equal(parseCursorMessage(bytes.buffer), null)
})

test("SHAPE truncated header (fewer than 17 bytes) returns null", () => {
    assert.equal(parseCursorMessage(new ArrayBuffer(CURSOR_SHAPE_HEADER_LEN - 1)), null)
})

// ── Malformed input ─────────────────────────────────────────────────────────

test("empty buffer returns null", () => {
    assert.equal(parseCursorMessage(new ArrayBuffer(0)), null)
})

test("unknown kind returns null", () => {
    assert.equal(parseCursorMessage(new Uint8Array([0xFF]).buffer), null)
})

test("truncated POS (17 of 18 bytes) returns null", () => {
    assert.equal(parseCursorMessage(new ArrayBuffer(CURSOR_POS_LEN - 1)), null)
})
