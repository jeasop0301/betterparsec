import assert from "node:assert/strict"
import test from "node:test"

import { parseQuMessage, encodeSubscribe, encodeBudget } from "../dist/stream/video/qu_wire.js"
import { crc32 } from "../dist/stream/video/crc32.js"

// ── Helpers ───────────────────────────────────────────────────────────────

function buf(...bytes) {
    return new Uint8Array(bytes).buffer
}

function u16le(v) { return [v & 0xFF, (v >> 8) & 0xFF] }
function u32le(v) {
    v = v >>> 0
    return [v & 0xFF, (v >> 8) & 0xFF, (v >> 16) & 0xFF, (v >> 24) & 0xFF]
}

function asBytes(ab) {
    return Array.from(new Uint8Array(ab))
}

// ── CRC-32 known vector ───────────────────────────────────────────────────

test("crc32: known vector '123456789' === 0xCBF43926", () => {
    const data = new Uint8Array([49,50,51,52,53,54,55,56,57]) // "123456789" ASCII
    assert.equal(crc32(data), 0xCBF43926)
})

test("crc32: empty input === 0x00000000", () => {
    assert.equal(crc32(new Uint8Array([])), 0x00000000)
})

test("crc32: single byte 0x00", () => {
    assert.equal(crc32(new Uint8Array([0x00])), 0xD202EF8D)
})

// ── QU_CONFIG shared byte pin ─────────────────────────────────────────────

test("QU_CONFIG shared byte pin: tile 128x128, cols 15, rows 9, epoch 7", () => {
    const expected = [0x01, 0x80,0x00, 0x80,0x00, 0x0F,0x00, 0x09,0x00, 0x07,0x00,0x00,0x00]
    const b = buf(...expected)
    const msg = parseQuMessage(b)
    assert.ok(msg, "parseQuMessage returned null")
    assert.equal(msg.kind, 0x01)
    assert.equal(msg.tileW, 128)
    assert.equal(msg.tileH, 128)
    assert.equal(msg.gridCols, 15)
    assert.equal(msg.gridRows, 9)
    assert.equal(msg.epoch, 7)
})

// ── QU_TILE shared byte pin ───────────────────────────────────────────────

test("QU_TILE shared byte pin: epoch 7, col 3, row 2, format 0, flags 0, crc 0xDEADBEEF, payload [0xAA,0xBB]", () => {
    const expected = [
        0x02,
        0x07,0x00,0x00,0x00,   // epoch 7
        0x03,0x00,             // col 3
        0x02,0x00,             // row 2
        0x00,                  // format 0
        0x00,                  // flags 0
        0xEF,0xBE,0xAD,0xDE,   // crc32 0xDEADBEEF LE
        0x02,0x00,0x00,0x00,   // payload_len 2
        0xAA,0xBB,             // payload
    ]
    const b = buf(...expected)
    const msg = parseQuMessage(b)
    assert.ok(msg, "parseQuMessage returned null")
    assert.equal(msg.kind, 0x02)
    assert.equal(msg.epoch, 7)
    assert.equal(msg.col, 3)
    assert.equal(msg.row, 2)
    assert.equal(msg.format, 0)
    assert.equal(msg.flags, 0)
    assert.equal(msg.crc32Bgra, 0xDEADBEEF >>> 0)
    assert.deepEqual(Array.from(msg.payload), [0xAA, 0xBB])
})

// ── QU_SUBSCRIBE shared byte pin ─────────────────────────────────────────

test("encodeSubscribe shared byte pin: [0x81, 0x01]", () => {
    assert.deepEqual(asBytes(encodeSubscribe()), [0x81, 0x01])
})

test("parseQuMessage QU_SUBSCRIBE: roundtrip", () => {
    const b = encodeSubscribe()
    const msg = parseQuMessage(b)
    assert.ok(msg)
    assert.equal(msg.kind, 0x81)
    assert.equal(msg.version, 1)
})

// ── QU_BUDGET shared byte pin ─────────────────────────────────────────────

test("encodeBudget shared byte pin: 4000 => [0x82, 0xA0, 0x0F, 0x00, 0x00]", () => {
    assert.deepEqual(asBytes(encodeBudget(4000)), [0x82, 0xA0, 0x0F, 0x00, 0x00])
})

test("encodeBudget 0 (pause)", () => {
    assert.deepEqual(asBytes(encodeBudget(0)), [0x82, 0x00, 0x00, 0x00, 0x00])
})

test("parseQuMessage QU_BUDGET: roundtrip 4000", () => {
    const b = encodeBudget(4000)
    const msg = parseQuMessage(b)
    assert.ok(msg)
    assert.equal(msg.kind, 0x82)
    assert.equal(msg.kbps, 4000)
})

// ── QU_INVALIDATE ─────────────────────────────────────────────────────────

test("QU_INVALIDATE: parse 2-tile message", () => {
    const b = buf(
        0x03,
        ...u32le(42),        // epoch
        ...u16le(2),         // count
        ...u16le(1), ...u16le(3),  // tile (1,3)
        ...u16le(7), ...u16le(0),  // tile (7,0)
    )
    const msg = parseQuMessage(b)
    assert.ok(msg)
    assert.equal(msg.kind, 0x03)
    assert.equal(msg.epoch, 42)
    assert.equal(msg.tiles.length, 2)
    assert.deepEqual(msg.tiles[0], { col: 1, row: 3 })
    assert.deepEqual(msg.tiles[1], { col: 7, row: 0 })
})

test("QU_INVALIDATE: 0 tiles is valid", () => {
    const b = buf(0x03, ...u32le(1), ...u16le(0))
    const msg = parseQuMessage(b)
    assert.ok(msg)
    assert.equal(msg.kind, 0x03)
    assert.equal(msg.tiles.length, 0)
})

// ── QU_EPOCH ─────────────────────────────────────────────────────────────

test("QU_EPOCH: parse epoch 99", () => {
    const b = buf(0x04, ...u32le(99))
    const msg = parseQuMessage(b)
    assert.ok(msg)
    assert.equal(msg.kind, 0x04)
    assert.equal(msg.epoch, 99)
})

// ── Roundtrips ─────────────────────────────────────────────────────────────

test("QU_CONFIG roundtrip", () => {
    const expected = [0x01, 0x80,0x00, 0x80,0x00, 0x0F,0x00, 0x09,0x00, 0x07,0x00,0x00,0x00]
    const msg = parseQuMessage(buf(...expected))
    assert.ok(msg)
    assert.equal(msg.kind, 0x01)
    assert.equal(msg.tileW, 128)
    assert.equal(msg.epoch, 7)
})

test("QU_BUDGET roundtrip max u32", () => {
    const b = encodeBudget(0xFFFFFFFF)
    const msg = parseQuMessage(b)
    assert.ok(msg)
    assert.equal(msg.kbps, 0xFFFFFFFF)
})

// ── Malformed: truncated per message type ────────────────────────────────

test("malformed: empty buffer returns null", () => {
    assert.equal(parseQuMessage(buf()), null)
})

test("malformed: QU_CONFIG truncated at 12 bytes (needs 13)", () => {
    const b = buf(0x01, 0x80,0x00, 0x80,0x00, 0x0F,0x00, 0x09,0x00, 0x07,0x00,0x00)
    assert.equal(parseQuMessage(b), null)
})

test("malformed: QU_TILE header truncated at 18 bytes (needs 19+payload)", () => {
    const b = buf(0x02, 0x07,0x00,0x00,0x00, 0x03,0x00, 0x02,0x00, 0x00, 0x00,
                  0xEF,0xBE,0xAD,0xDE, 0x01,0x00,0x00)  // missing payload_len byte and payload
    assert.equal(parseQuMessage(b), null)
})

test("malformed: QU_TILE payload_len mismatch (declares 5 but only 2 bytes follow)", () => {
    const b = buf(
        0x02,
        0x07,0x00,0x00,0x00,  // epoch
        0x00,0x00,             // col
        0x00,0x00,             // row
        0x00,                  // format
        0x00,                  // flags
        0x00,0x00,0x00,0x00,  // crc
        0x05,0x00,0x00,0x00,  // payload_len=5
        0xAA,0xBB,            // only 2 bytes
    )
    assert.equal(parseQuMessage(b), null)
})

test("QU_TILE trailing bytes accepted (payload_len=1 but 3 payload bytes present — Rust-compat)", () => {
    // The Rust relay accepts trailing bytes after declared payload_len (alignment
    // padding / extension fields).  The TS parser must match this behaviour so
    // frames forwarded verbatim through the relay are not silently discarded.
    const b = buf(
        0x02,
        0x01,0x00,0x00,0x00,  // epoch
        0x00,0x00, 0x00,0x00, // col, row
        0x00, 0x00,            // format, flags
        0x00,0x00,0x00,0x00,  // crc
        0x01,0x00,0x00,0x00,  // payload_len=1
        0xAA,0xBB,0xCC,       // 3 bytes; only 0xAA is the payload
    )
    const msg = parseQuMessage(b)
    assert.ok(msg, "QU_TILE with trailing bytes must parse successfully")
    assert.equal(msg.kind, 0x02)
    assert.deepEqual(Array.from(msg.payload), [0xAA], "payload sliced to declared length")
})

test("malformed: QU_INVALIDATE count mismatch (count=3 but only 2 tile pairs)", () => {
    const b = buf(
        0x03,
        ...u32le(1),   // epoch
        ...u16le(3),   // count=3
        ...u16le(0), ...u16le(0),  // tile 0
        ...u16le(1), ...u16le(1),  // tile 1  (missing tile 2)
    )
    assert.equal(parseQuMessage(b), null)
})

test("malformed: QU_EPOCH truncated at 4 bytes (needs 5)", () => {
    const b = buf(0x04, 0x01,0x00,0x00)
    assert.equal(parseQuMessage(b), null)
})

test("malformed: QU_SUBSCRIBE truncated at 1 byte (needs 2)", () => {
    const b = buf(0x81)
    assert.equal(parseQuMessage(b), null)
})

test("malformed: QU_BUDGET truncated at 4 bytes (needs 5)", () => {
    const b = buf(0x82, 0xA0,0x0F,0x00)
    assert.equal(parseQuMessage(b), null)
})

test("malformed: unknown kind returns null", () => {
    assert.equal(parseQuMessage(buf(0x99, 0x00, 0x00)), null)
})
