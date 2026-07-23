import assert from "node:assert/strict"
import test from "node:test"

import {
    negotiate,
    bestCommonCodec,
    transferIsHdr,
    sdrBaseline,
    CODEC_BIT,
    encodeHdrMetadata,
    decodeHdrMetadata,
    HDR_METADATA_WIRE_LEN,
} from "../dist/stream/media_caps.js"

// Truth vectors in lockstep with common/src/media_caps.rs `tests`.

const AV1 = CODEC_BIT.av1
const HEVC = CODEC_BIT.hevc
const H264 = CODEC_BIT.h264

function caps(codecs, chroma444, bit10, hdr, refresh, vrr) {
    return { codecs, chroma444, bit10, hdr, maxRefreshMhz: refresh, vrr }
}

function hdrAv1_444_10bit_4k144() {
    return {
        codec: "av1",
        chroma: "yuv444",
        bitDepth: 10,
        primaries: "bt2020",
        transfer: "pq",
        matrix: "bt2020ncl",
        range: "full",
        hdr: { maxCll: 1000, maxFall: 400, maxLuminance: 10000000, minLuminance: 1 },
        width: 3840,
        height: 2160,
        refreshMhz: 144000,
        vrr: true,
    }
}

test("descriptor helpers", () => {
    assert.equal(transferIsHdr("pq"), true)
    assert.equal(transferIsHdr("bt709"), false)
    assert.equal(bestCommonCodec(0), "h264")
    assert.equal(bestCommonCodec(AV1 | HEVC), "av1")
    assert.equal(bestCommonCodec(HEVC | H264), "hevc")
})

test("full match negotiates the exact mode", () => {
    const full = caps(AV1 | HEVC | H264, true, true, true, 144000, true)
    const req = hdrAv1_444_10bit_4k144()
    const { mode, reason } = negotiate(full, full, req)
    assert.equal(reason, null)
    assert.deepEqual(mode, req)
})

test("HDR falls back deterministically to SDR", () => {
    const hdrClient = caps(AV1 | HEVC | H264, true, true, true, 144000, true)
    const sdrHost = caps(AV1 | HEVC | H264, true, true, false, 144000, true)
    const { mode, reason } = negotiate(hdrClient, sdrHost, hdrAv1_444_10bit_4k144())
    assert.equal(reason, "unsupported")
    assert.equal(mode.hdr, null)
    assert.equal(mode.transfer, "bt709")
    assert.equal(mode.primaries, "bt709")
    assert.equal(mode.matrix, "bt709")
    assert.equal(mode.range, "limited")
    assert.equal(mode.codec, "av1")
    assert.equal(mode.chroma, "yuv444")
})

test("codec/chroma/bit-depth/refresh/vrr downgrade matrix", () => {
    const client = caps(AV1 | HEVC | H264, true, true, false, 144000, true)
    const host = caps(HEVC | H264, false, false, false, 120000, false)
    const req = { ...hdrAv1_444_10bit_4k144(), transfer: "bt709", hdr: null }
    const { mode, reason } = negotiate(client, host, req)
    assert.equal(reason, "capability_mismatch")
    assert.equal(mode.codec, "hevc")
    assert.equal(mode.chroma, "yuv420")
    assert.equal(mode.bitDepth, 8)
    assert.equal(mode.refreshMhz, 120000)
    assert.equal(mode.vrr, false)
})

test("H264 baseline survives when nothing above it is shared", () => {
    const client = caps(AV1 | H264, false, false, false, 60000, false)
    const host = caps(H264, false, false, false, 60000, false)
    const req = { ...sdrBaseline(), codec: "av1" }
    const { mode, reason } = negotiate(client, host, req)
    assert.equal(mode.codec, "h264")
    assert.equal(reason, "capability_mismatch")
})

test("HDR metadata wire round trip and byte pin", () => {
    const hdr = { maxCll: 1000, maxFall: 400, maxLuminance: 10000000, minLuminance: 5 }
    const bytes = encodeHdrMetadata(hdr)
    assert.deepEqual(
        [...bytes],
        [0xe8, 0x03, 0x90, 0x01, 0x80, 0x96, 0x98, 0x00, 0x05, 0x00, 0x00, 0x00],
    )
    assert.deepEqual(decodeHdrMetadata(bytes.buffer), hdr)
    assert.equal(decodeHdrMetadata(bytes.slice(0, HDR_METADATA_WIRE_LEN - 1).buffer), null)
})
