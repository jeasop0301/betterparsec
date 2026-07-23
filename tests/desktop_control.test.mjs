import assert from "node:assert/strict"
import test from "node:test"

import {
    encodeEnvelope,
    decodeEnvelope,
    generationSupersedes,
    downgradeReason,
    OwnershipLease,
    CONTROL_HEADER_LEN,
    CONTROL_MAX_PAYLOAD,
    CONTROL_DOMAIN_DISPLAY,
    DISPLAY_KIND_MODE_REQUEST,
    encodeModeRequest,
    decodeModeRequest,
    encodeModeResult,
    decodeModeResult,
    MODE_REQUEST_LEN,
    MODE_RESULT_LEN,
    encodePrivacyRequest,
    decodePrivacyRequest,
    encodePrivacyState,
    decodePrivacyState,
    PROTECT_BLANK_DISPLAY,
    PROTECT_BLOCK_LOCAL_INPUT,
    PROTECT_ALL,
    PRIVACY_REQUEST_LEN,
    PRIVACY_STATE_LEN,
} from "../dist/stream/desktop_control.js"

import {
    encodeCursorAuthority,
    decodeCursorAuthority,
    encodeClipboardOffer,
    decodeClipboardOffer,
    encodeClipboardRequest,
    decodeClipboardRequest,
    CLIP_TEXT,
    CLIP_PNG,
    CLIP_ALL,
    encodeFileOffer,
    decodeFileOffer,
    encodeFileCancel,
    decodeFileCancel,
    encodeFileAccept,
    decodeFileAccept,
    encodeFileProgress,
    decodeFileProgress,
    FT_OFFER_HEADER_LEN,
    FT_MAX_NAME_LEN,
} from "../dist/stream/desktop_control.js"

import { OutputSelection } from "../dist/stream/desktop_control.js"

// ── Shared byte pin with desktop_control.rs `envelope_byte_pin_and_round_trip`
//    (change both or neither) ────────────────────────────────────────────────

test("envelope shared byte pin: Display, kind=0x0102, gen=0x04030201, payload AA BB CC", () => {
    const bytes = encodeEnvelope(CONTROL_DOMAIN_DISPLAY, 0x0102, 0x04030201, new Uint8Array([0xaa, 0xbb, 0xcc]))
    assert.deepEqual(
        [...bytes],
        [0x01, 0x02, 0x02, 0x01, 0x01, 0x02, 0x03, 0x04, 0x03, 0x00, 0x00, 0x00, 0xaa, 0xbb, 0xcc],
    )
    const env = decodeEnvelope(bytes.buffer)
    assert.equal(env.domain, CONTROL_DOMAIN_DISPLAY)
    assert.equal(env.kind, 0x0102)
    assert.equal(env.generation, 0x04030201)
    assert.deepEqual([...env.payload], [0xaa, 0xbb, 0xcc])
})

test("generation supersede is wrap-safe (RFC 1982)", () => {
    assert.equal(generationSupersedes(5, 3), true)
    assert.equal(generationSupersedes(3, 5), false)
    assert.equal(generationSupersedes(7, 7), false)
    // 0 is one step after u32::MAX across the wrap.
    assert.equal(generationSupersedes(0, 0xffffffff), true)
    assert.equal(generationSupersedes(0xffffffff, 0), false)
    // Half-range boundary.
    assert.equal(generationSupersedes(0x7fffffff, 0), true)
    assert.equal(generationSupersedes(0x80000000, 0), false)
})

test("ownership lease rejects stale without mutating", () => {
    const lease = new OwnershipLease()
    assert.equal(lease.admit(10), true)
    assert.equal(lease.admit(10), true) // idempotent refresh
    assert.equal(lease.admit(11), true)
    assert.equal(lease.admit(10), false) // stale
    assert.equal(lease.current(), 11)
    lease.release()
    assert.equal(lease.current(), null)
    assert.equal(lease.admit(4), true)
    // Wrap-around takeover.
    const wrap = new OwnershipLease()
    assert.equal(wrap.admit(0xffffffff), true)
    assert.equal(wrap.admit(0), true)
    assert.equal(wrap.current(), 0)
})

test("downgrade reason decode is forward-compatible", () => {
    assert.equal(downgradeReason(0), null)
    assert.deepEqual(downgradeReason(4), { name: "stale_generation", code: 4 })
    assert.deepEqual(downgradeReason(200), { name: "unknown", code: 200 })
})

test("envelope rejects malformed and tolerates trailing bytes", () => {
    const good = encodeEnvelope(1, 1, 1, new Uint8Array([9, 9]))
    // Truncated header.
    assert.equal(decodeEnvelope(good.slice(0, CONTROL_HEADER_LEN - 1).buffer), null)
    // Unknown version.
    const badVersion = good.slice()
    badVersion[0] = 2
    assert.equal(decodeEnvelope(badVersion.buffer), null)
    // Body shorter than declared.
    assert.equal(decodeEnvelope(good.slice(0, good.length - 1).buffer), null)
    // Oversized declared payload_len.
    const oversize = good.slice()
    new DataView(oversize.buffer).setUint32(8, CONTROL_MAX_PAYLOAD + 1, true)
    assert.equal(decodeEnvelope(oversize.buffer), null)
    // Trailing bytes tolerated.
    const trailing = new Uint8Array(good.length + 2)
    trailing.set(good)
    trailing[good.length] = 0xff
    trailing[good.length + 1] = 0xff
    const env = decodeEnvelope(trailing.buffer)
    assert.deepEqual([...env.payload], [9, 9])
})

test("encode refuses oversized payload", () => {
    assert.equal(encodeEnvelope(4, 0, 0, new Uint8Array(CONTROL_MAX_PAYLOAD + 1)), null)
    assert.notEqual(encodeEnvelope(4, 0, 0, new Uint8Array(CONTROL_MAX_PAYLOAD)), null)
})

// ── Display domain byte pins (mirror of desktop_control::display) ──────────

test("display mode request shared byte pin: output 2, 3840x2160@144000mHz", () => {
    const bytes = encodeModeRequest({ outputId: 2, width: 3840, height: 2160, refreshMhz: 144000 })
    assert.deepEqual(
        [...bytes],
        [0x02, 0x00, 0x00, 0x0f, 0x70, 0x08, 0x80, 0x32, 0x02, 0x00],
    )
    assert.equal(bytes.length, MODE_REQUEST_LEN)
    assert.deepEqual(decodeModeRequest(bytes.buffer), {
        outputId: 2,
        width: 3840,
        height: 2160,
        refreshMhz: 144000,
    })
    // Kind constant is stable across the two sides.
    assert.equal(DISPLAY_KIND_MODE_REQUEST, 1)
    // Truncation rejected.
    assert.equal(decodeModeRequest(bytes.slice(0, MODE_REQUEST_LEN - 1).buffer), null)
})

test("display mode result shared byte pin: downgraded, capability_mismatch", () => {
    const bytes = encodeModeResult({
        outputId: 1,
        width: 2560,
        height: 1440,
        refreshMhz: 60000,
        status: "downgraded",
        reason: { name: "capability_mismatch", code: 2 },
    })
    assert.deepEqual(
        [...bytes],
        [0x01, 0x00, 0x00, 0x0a, 0xa0, 0x05, 0x60, 0xea, 0x00, 0x00, 0x01, 0x02],
    )
    assert.equal(bytes.length, MODE_RESULT_LEN)
    const res = decodeModeResult(bytes.buffer)
    assert.equal(res.status, "downgraded")
    assert.deepEqual(res.reason, { name: "capability_mismatch", code: 2 })
    // Applied with no reason encodes 0,0 in the trailing bytes.
    const applied = encodeModeResult({ outputId: 1, width: 2560, height: 1440, refreshMhz: 60000, status: "applied", reason: null })
    assert.equal(applied[10], 0)
    assert.equal(applied[11], 0)
    assert.equal(decodeModeResult(applied.buffer).reason, null)
    // Unknown status byte is rejected.
    const bad = bytes.slice()
    bad[10] = 9
    assert.equal(decodeModeResult(bad.buffer), null)
})

// ── Privacy domain byte pins (mirror of desktop_control::privacy) ──────────

test("privacy request round trip and reserved-bit rejection", () => {
    const bytes = encodePrivacyRequest({ enable: true, protections: PROTECT_BLANK_DISPLAY | PROTECT_BLOCK_LOCAL_INPUT })
    assert.deepEqual([...bytes], [0x01, 0x03])
    assert.equal(bytes.length, PRIVACY_REQUEST_LEN)
    assert.deepEqual(decodePrivacyRequest(bytes.buffer), { enable: true, protections: PROTECT_ALL })
    assert.deepEqual(decodePrivacyRequest(new Uint8Array([0, 0]).buffer), { enable: false, protections: 0 })
    // Reserved bit rejected; truncation rejected.
    assert.equal(decodePrivacyRequest(new Uint8Array([1, 0x80]).buffer), null)
    assert.equal(decodePrivacyRequest(bytes.slice(0, PRIVACY_REQUEST_LEN - 1).buffer), null)
})

test("privacy state shared byte pin: downgraded, policy_denied", () => {
    const bytes = encodePrivacyState({
        active: true,
        protectionsEffective: PROTECT_BLANK_DISPLAY,
        status: "downgraded",
        reason: { name: "policy_denied", code: 5 },
    })
    assert.deepEqual([...bytes], [0x01, 0x01, 0x01, 0x05])
    assert.equal(bytes.length, PRIVACY_STATE_LEN)
    const state = decodePrivacyState(bytes.buffer)
    assert.equal(state.status, "downgraded")
    assert.deepEqual(state.reason, { name: "policy_denied", code: 5 })
    // Applied with no reason.
    const applied = encodePrivacyState({ active: true, protectionsEffective: PROTECT_ALL, status: "applied", reason: null })
    assert.deepEqual([...applied], [0x01, 0x03, 0x00, 0x00])
    assert.equal(decodePrivacyState(applied.buffer).reason, null)
    // Unknown status rejected.
    const bad = bytes.slice()
    bad[2] = 9
    assert.equal(decodePrivacyState(bad.buffer), null)
})

// ── Cursor / clipboard / file-transfer domains ─────────────────────────────

test("cursor authority round trip and unknown owner", () => {
    assert.deepEqual([...encodeCursorAuthority("host")], [0x00])
    assert.deepEqual([...encodeCursorAuthority("client")], [0x01])
    assert.equal(decodeCursorAuthority(new Uint8Array([0]).buffer), "host")
    assert.equal(decodeCursorAuthority(new Uint8Array([1]).buffer), "client")
    assert.equal(decodeCursorAuthority(new Uint8Array([2]).buffer), null)
    assert.equal(decodeCursorAuthority(new Uint8Array([]).buffer), null)
})

test("clipboard offer masks unknown bits and request is single kind", () => {
    assert.deepEqual([...encodeClipboardOffer(CLIP_ALL | 0x80)], [CLIP_ALL])
    assert.equal(decodeClipboardOffer(new Uint8Array([0xff]).buffer), CLIP_ALL)
    assert.equal(decodeClipboardOffer(new Uint8Array([]).buffer), null)
    assert.equal(decodeClipboardRequest(encodeClipboardRequest(CLIP_PNG).buffer), CLIP_PNG)
    assert.equal(decodeClipboardRequest(new Uint8Array([CLIP_TEXT | CLIP_PNG]).buffer), null)
    assert.equal(decodeClipboardRequest(new Uint8Array([0]).buffer), null)
    assert.equal(decodeClipboardRequest(new Uint8Array([0x80]).buffer), null)
})

test("file offer shared byte pin and bounds", () => {
    const offer = { transferId: 0x0a0b0c0d, totalSize: 5000000000, name: "movie.mkv" }
    const bytes = encodeFileOffer(offer)
    assert.deepEqual([...bytes.slice(0, 4)], [0x0d, 0x0c, 0x0b, 0x0a])
    assert.deepEqual([...bytes.slice(12, 14)], [0x09, 0x00])
    assert.equal(new TextDecoder().decode(bytes.slice(14)), "movie.mkv")
    assert.deepEqual(decodeFileOffer(bytes.buffer), offer)
    // Truncated header/name rejected.
    assert.equal(decodeFileOffer(bytes.slice(0, FT_OFFER_HEADER_LEN - 1).buffer), null)
    assert.equal(decodeFileOffer(bytes.slice(0, bytes.length - 1).buffer), null)
    // Over-cap name refused.
    assert.equal(encodeFileOffer({ transferId: 1, totalSize: 0, name: "x".repeat(FT_MAX_NAME_LEN + 1) }), null)
    // Invalid UTF-8 rejected.
    const badUtf8 = encodeFileOffer({ transferId: 1, totalSize: 0, name: "ab" })
    badUtf8[14] = 0xff
    assert.equal(decodeFileOffer(badUtf8.buffer), null)
})

test("file accept/cancel/progress round trip", () => {
    assert.equal(decodeFileAccept(encodeFileAccept(42).buffer), 42)
    assert.equal(decodeFileAccept(new Uint8Array([0, 0, 0]).buffer), null)

    const cancel = encodeFileCancel(7, { name: "policy_denied", code: 5 })
    assert.deepEqual([...cancel], [0x07, 0x00, 0x00, 0x00, 0x05])
    assert.deepEqual(decodeFileCancel(cancel.buffer), { transferId: 7, reason: { name: "policy_denied", code: 5 } })
    assert.deepEqual(decodeFileCancel(encodeFileCancel(7, null).buffer), { transferId: 7, reason: null })

    const prog = encodeFileProgress(9, 1234567)
    assert.deepEqual(decodeFileProgress(prog.buffer), { transferId: 9, bytesDone: 1234567 })
    assert.equal(decodeFileProgress(new Uint8Array(11).buffer), null)
})

// ── Output selection (mirror of desktop_control::display::OutputSelection) ──

test("output selection tracks requested vs applied and rolls back", () => {
    const sel = new OutputSelection()
    assert.equal(sel.applied(), null)
    assert.equal(sel.pending(), null)

    const req = { outputId: 1, width: 3840, height: 2160, refreshMhz: 144000 }
    sel.request(req)
    assert.deepEqual(sel.pending(), req)

    assert.equal(sel.reconcile({ outputId: 1, width: 3840, height: 2160, refreshMhz: 144000, status: "applied", reason: null }), "applied")
    assert.equal(sel.pending(), null)
    assert.equal(sel.applied().width, 3840)

    // Rejected rolls back to the previous applied mode.
    sel.request({ outputId: 1, width: 7680, height: 4320, refreshMhz: 60000 })
    assert.equal(sel.reconcile({ outputId: 1, width: 0, height: 0, refreshMhz: 0, status: "rejected", reason: { name: "unsupported", code: 1 } }), "rejected")
    assert.equal(sel.applied().width, 3840)

    // Downgraded adopts the effective mode.
    sel.request({ outputId: 1, width: 2560, height: 1440, refreshMhz: 240000 })
    sel.reconcile({ outputId: 1, width: 2560, height: 1440, refreshMhz: 144000, status: "downgraded", reason: { name: "capability_mismatch", code: 2 } })
    assert.equal(sel.applied().refreshMhz, 144000)
})

test("output selection hot-unplug falls back only for the active output", () => {
    const sel = new OutputSelection()
    sel.request({ outputId: 2, width: 1920, height: 1080, refreshMhz: 60000 })
    sel.reconcile({ outputId: 2, width: 1920, height: 1080, refreshMhz: 60000, status: "applied", reason: null })
    assert.equal(sel.onOutputRemoved(5, 0), null)
    assert.notEqual(sel.applied(), null)
    const fallback = sel.onOutputRemoved(2, 0)
    assert.equal(fallback.outputId, 0)
    assert.equal(sel.applied(), null)
    assert.deepEqual(sel.pending(), fallback)
})
