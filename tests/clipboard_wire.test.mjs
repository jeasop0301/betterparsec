import assert from "node:assert/strict"
import test from "node:test"

import { encodeClipboardText, decodeClipboardText, CLIPBOARD_MAX_LEN } from "../dist/stream/clipboard_wire.js"
import {
    encodeClipboardImage,
    decodeClipboardImage,
    CLIPBOARD_IMAGE_MAX_LEN,
} from "../dist/stream/clipboard_wire.js"

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

test("decode rejects invalid UTF-8 in the text body (parity with Rust from_utf8)", () => {
    // Header kind=TEXT, len=2, body = 0xFF 0xFE (not valid UTF-8). A non-fatal
    // decoder would return U+FFFD; the Rust peer drops it, so must the TS peer.
    const bytes = new Uint8Array([0x00, 0x02, 0x00, 0x00, 0x00, 0xff, 0xfe])
    assert.equal(decodeClipboardText(bytes.buffer), null)
})

// ── Image (PNG) wire (shared byte pin with clipboard.rs image) ─────────────

const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x42])

test("IMAGE shared byte pin and round trip", () => {
    const buf = encodeClipboardImage(1920, 1080, PNG)
    assert.deepEqual(
        new Uint8Array(buf).slice(0, 9),
        new Uint8Array([0x01, 0x80, 0x07, 0x38, 0x04, 0x09, 0x00, 0x00, 0x00]),
    )
    const decoded = decodeClipboardImage(buf)
    assert.equal(decoded.width, 1920)
    assert.equal(decoded.height, 1080)
    assert.deepEqual([...decoded.png], [...PNG])
})

test("IMAGE encode rejects non-PNG and oversize", () => {
    assert.equal(encodeClipboardImage(1, 1, new Uint8Array([0, 1, 2, 3, 4, 5, 6, 7, 8])), null)
    const big = new Uint8Array(CLIPBOARD_IMAGE_MAX_LEN + 1)
    big.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
    assert.equal(encodeClipboardImage(1, 1, big), null)
})

test("IMAGE decode rejects malformed and ignores text", () => {
    const buf = encodeClipboardImage(2, 2, new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]))
    // Wrong kind byte.
    assert.equal(decodeClipboardImage(new Uint8Array([0, 0, 0, 0, 0, 0, 0, 0, 0]).buffer), null)
    // Truncated header/body.
    assert.equal(decodeClipboardImage(buf.slice(0, 8)), null)
    assert.equal(decodeClipboardImage(buf.slice(0, buf.byteLength - 1)), null)
    // The text decoder ignores an image frame.
    assert.equal(decodeClipboardText(buf), null)
    // The image decoder ignores a text frame.
    assert.equal(decodeClipboardImage(encodeClipboardText("hi")), null)
})
