import assert from "node:assert/strict"
import test from "node:test"

import { encodeClipboardText, decodeClipboardText, CLIPBOARD_MAX_LEN } from "../dist/stream/clipboard_wire.js"

// ── Shared byte pin with clipboard.rs (change both or neither) ─────────────

test("TEXT shared byte pin: \"hi\"", () => {
    const encoded = encodeClipboardText("hi")
    assert.deepEqual(
        new Uint8Array(encoded),
        new Uint8Array([0x00, 0x02, 0x00, 0x00, 0x00, 0x68, 0x69])
    )
})

test("decode of the pinned bytes round-trips to \"hi\"", () => {
    const bytes = new Uint8Array([0x00, 0x02, 0x00, 0x00, 0x00, 0x68, 0x69])
    assert.equal(decodeClipboardText(bytes.buffer), "hi")
})

// ── Roundtrip ────────────────────────────────────────────────────────────

test("encode/decode roundtrip: ascii text", () => {
    const encoded = encodeClipboardText("hello, world")
    assert.equal(decodeClipboardText(encoded), "hello, world")
})

test("encode/decode roundtrip: empty string", () => {
    const encoded = encodeClipboardText("")
    assert.equal(decodeClipboardText(encoded), "")
})

test("encode/decode roundtrip: multi-byte utf-8", () => {
    const text = "héllo wörld 日本語 🎮"
    const encoded = encodeClipboardText(text)
    assert.equal(decodeClipboardText(encoded), text)
})

// ── Cap rejection ────────────────────────────────────────────────────────

test("encode refuses text at the cap boundary + 1 byte", () => {
    const oversized = "a".repeat(CLIPBOARD_MAX_LEN + 1)
    assert.equal(encodeClipboardText(oversized), null)
})

test("encode accepts text exactly at the cap boundary", () => {
    const atCap = "a".repeat(CLIPBOARD_MAX_LEN)
    const encoded = encodeClipboardText(atCap)
    assert.notEqual(encoded, null)
    assert.equal(decodeClipboardText(encoded), atCap)
})

test("decode rejects a declared length over the cap", () => {
    const out = new Uint8Array(5)
    const view = new DataView(out.buffer)
    out[0] = 0x00
    view.setUint32(1, CLIPBOARD_MAX_LEN + 1, true)
    assert.equal(decodeClipboardText(out.buffer), null)
})

// ── Unknown kind ─────────────────────────────────────────────────────────

test("decode ignores an unknown kind byte", () => {
    const bytes = new Uint8Array([0xFF, 0x00, 0x00, 0x00, 0x00])
    assert.equal(decodeClipboardText(bytes.buffer), null)
})

// ── Truncated / malformed ───────────────────────────────────────────────

test("decode rejects an empty buffer", () => {
    assert.equal(decodeClipboardText(new ArrayBuffer(0)), null)
})

test("decode rejects a header shorter than 5 bytes", () => {
    assert.equal(decodeClipboardText(new Uint8Array([0x00, 0x02, 0x00]).buffer), null)
})

test("decode rejects a body shorter than the declared length", () => {
    // Header claims 5 bytes of payload but only 2 are present.
    const bytes = new Uint8Array([0x00, 0x05, 0x00, 0x00, 0x00, 0x68, 0x69])
    assert.equal(decodeClipboardText(bytes.buffer), null)
})
