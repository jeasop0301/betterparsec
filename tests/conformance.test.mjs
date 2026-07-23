import assert from "node:assert/strict"
import test from "node:test"
import { readFileSync } from "node:fs"

import { encodeEnvelope, encodeModeRequest, encodePrivacyState, encodeCursorAuthority, encodeClipboardOffer, encodePrivacyRequest, encodeFileCancel } from "../dist/stream/desktop_control.js"
import { encodeHdrMetadata } from "../dist/stream/media_caps.js"

// Protocol conformance kit (G032): load the SAME canonical vectors the Rust
// conformance test loads (common/tests/conformance.rs) and assert the TS
// encoders produce byte-identical output. A client port passes conformance when
// it reproduces every expected_hex.

const doc = JSON.parse(readFileSync(new URL("./conformance/wire_vectors.json", import.meta.url), "utf8"))

function hexToBytes(hex) {
    const out = new Uint8Array(hex.length / 2)
    for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16)
    return out
}

function encodeVector(v) {
    switch (v.format) {
        case "control_envelope":
            return new Uint8Array(encodeEnvelope(v.domain, v.kind, v.generation, hexToBytes(v.payload_hex)))
        case "display_mode_request":
            return encodeModeRequest({ outputId: v.output_id, width: v.width, height: v.height, refreshMhz: v.refresh_mhz })
        case "privacy_state":
            return encodePrivacyState({
                active: v.active,
                protectionsEffective: v.protections,
                status: v.status,
                reason: v.reason ? { name: "", code: v.reason } : null,
            })
        case "hdr_metadata":
            return encodeHdrMetadata({ maxCll: v.max_cll, maxFall: v.max_fall, maxLuminance: v.max_luminance, minLuminance: v.min_luminance })
        case "cursor_authority":
            return encodeCursorAuthority(v.owner)
        case "clipboard_offer":
            return encodeClipboardOffer(v.kinds)
        case "privacy_request":
            return encodePrivacyRequest({ enable: v.enable, protections: v.protections })
        case "file_cancel":
            return encodeFileCancel(v.transfer_id, v.reason ? { name: "", code: v.reason } : null)
        default:
            throw new Error(`unknown conformance format ${v.format}`)
    }
}

test("wire vectors are byte-identical (TS side of the conformance kit)", () => {
    assert.ok(Array.isArray(doc.vectors) && doc.vectors.length > 0)
    for (const v of doc.vectors) {
        assert.deepEqual([...encodeVector(v)], [...hexToBytes(v.expected_hex)], `conformance vector mismatch: ${v.name}`)
    }
})
