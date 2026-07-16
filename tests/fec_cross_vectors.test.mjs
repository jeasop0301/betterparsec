// Cross-language vector tests for U2 FEC framing.
// Fixture: tests/fixtures/fec_vectors.json (generated + pinned by Rust).
//
// Test 1 — Decoder: parse non-dropped messages, feed FecDecoder, reassemble
//           frames, assert byte-equality with expected_frames.
// Test 2 — Encoder mirror: reimplement LCG, regenerate frames, run through
//           TS FecEncoder + chunkFrame, assert hex list == fixture.messages.

import { readFileSync } from "node:fs"
import { join, dirname } from "node:path"
import { fileURLToPath } from "node:url"
import assert from "node:assert/strict"
import test from "node:test"

import { FecEncoder, FecDecoder } from "../dist/stream/video/fec.js"
import {
    encodeSymbolMessage,
    encodeSymbolMessageV2,
    parseSymbolMessage,
    chunkFrame,
    CHUNK_HEADER_SIZE,
    crc32,
} from "../dist/stream/video/fec_wire.js"

// ── Fixture ───────────────────────────────────────────────────────────────

const __filename = fileURLToPath(import.meta.url)
const FIXTURE_PATH = join(dirname(__filename), "fixtures/fec_vectors.json")
const fixture = JSON.parse(readFileSync(FIXTURE_PATH, "utf8"))

// ── Helpers ───────────────────────────────────────────────────────────────

/** Decode lowercase hex string to Uint8Array. */
function hexToU8(hex) {
    const out = new Uint8Array(hex.length >> 1)
    for (let i = 0; i < hex.length; i += 2) {
        out[i >> 1] = parseInt(hex.slice(i, i + 2), 16)
    }
    return out
}

/** Encode Uint8Array to lowercase hex string. */
function u8ToHex(bytes) {
    return Array.from(bytes, b => b.toString(16).padStart(2, "0")).join("")
}

/**
 * LCG step mirroring streamer/src/fec.rs cross_vector_tests::lcg_byte.
 * state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223)
 * output byte = (state >> 24) as u8
 */
function lcgStep(state) {
    return (Math.imul(state >>> 0, 1664525) + 1013904223) >>> 0
}

/**
 * Generate `size` bytes via LCG starting from `startState`.
 * Returns { data: Uint8Array, endState: number }.
 */
function genFrameData(size, startState) {
    const data = new Uint8Array(size)
    let state = startState
    for (let i = 0; i < size; i++) {
        state = lcgStep(state)
        data[i] = (state >>> 24) & 0xFF
    }
    return { data, endState: state }
}

/**
 * Parse the 13-byte chunk header from a Uint8Array (source-symbol payload).
 * Returns null if too short.
 */
function parseChunkHdrFromU8(payload) {
    if (payload.byteLength < CHUNK_HEADER_SIZE) return null
    const v = new DataView(payload.buffer, payload.byteOffset, payload.byteLength)
    return {
        frameId:     v.getUint32(0, true),
        chunkIndex:  v.getUint16(4, true),
        chunkCount:  v.getUint16(6, true),
        isKey:       v.getUint8(8) !== 0,
        timestampUs: v.getUint32(9, true),
    }
}

// ── Test 1: Decoder correctness ───────────────────────────────────────────

test("fec_cross_vectors: decoder recovers all frames from non-dropped messages", () => {
    const { config } = fixture.meta
    const dec = new FecDecoder(config.window_max_symbols, config.window_max_bytes)
    const dropped = new Set(fixture.dropped_message_indices)

    // frame_id -> Array<{ chunkIndex: number, fragment: Uint8Array }>
    const collectedChunks = new Map()

    for (let i = 0; i < fixture.messages.length; i++) {
        if (dropped.has(i)) continue

        const msgBytes = hexToU8(fixture.messages[i])
        const sym = parseSymbolMessage(msgBytes.buffer)
        assert.ok(sym !== null, `message[${i}] must parse as a valid FecSymbol`)

        for (const ev of dec.pushSymbol(sym)) {
            if (ev.kind !== "recovered") continue
            // ev.payload is the source-symbol chunk payload (header + fragment)
            const hdr = parseChunkHdrFromU8(ev.payload)
            if (hdr === null) continue
            const fragment = ev.payload.slice(CHUNK_HEADER_SIZE)
            if (!collectedChunks.has(hdr.frameId)) {
                collectedChunks.set(hdr.frameId, [])
            }
            collectedChunks.get(hdr.frameId).push({ chunkIndex: hdr.chunkIndex, fragment })
        }
    }

    // Reassemble each frame and assert byte-equality with expected_frames
    for (const ef of fixture.expected_frames) {
        const frameChunks = collectedChunks.get(ef.frame_id)
        assert.ok(
            frameChunks !== undefined && frameChunks.length > 0,
            `frame ${ef.frame_id} must have at least one recovered chunk`,
        )
        frameChunks.sort((a, b) => a.chunkIndex - b.chunkIndex)

        const totalLen = frameChunks.reduce((s, c) => s + c.fragment.length, 0)
        const reassembled = new Uint8Array(totalLen)
        let off = 0
        for (const { fragment } of frameChunks) {
            reassembled.set(fragment, off)
            off += fragment.length
        }

        assert.strictEqual(
            u8ToHex(reassembled),
            ef.data_hex,
            `frame ${ef.frame_id}: reassembled bytes do not match expected_frames.data_hex`,
        )
    }
})

// ── Test 2: Encoder mirror ────────────────────────────────────────────────

test("fec_cross_vectors: TS encoder produces identical hex to committed Rust fixture", () => {
    const { config, frames: frameSpecs } = fixture.meta

    const fecCfg = {
        redundancyNumerator:   config.redundancy_numerator,
        redundancyDenominator: config.redundancy_denominator,
        windowMaxSymbols:      config.window_max_symbols,
        windowMaxBytes:        config.window_max_bytes,
    }

    const SEED = 0x00C0FFEE
    let lcgState = SEED
    let seq = 0
    const enc = new FecEncoder(fecCfg)
    const hexMessages = []

    for (const sp of frameSpecs) {
        // Regenerate frame data using the same LCG
        const { data: frameData, endState } = genFrameData(sp.size, lcgState)
        lcgState = endState

        const isKey = sp.frame_type === "key"
        // Use the TS chunkFrame (same implementation as Rust chunk_frame)
        const chunkBufs = chunkFrame(sp.frame_id, isKey, sp.timestamp_us, frameData)

        for (const chunkAb of chunkBufs) {
            const chunk = new Uint8Array(chunkAb)
            const out = enc.pushSource(seq, chunk)
            // Source first, then repairs — matches Rust emission order
            hexMessages.push(u8ToHex(new Uint8Array(encodeSymbolMessage(out.source))))
            for (const r of out.repairs) {
                hexMessages.push(u8ToHex(new Uint8Array(encodeSymbolMessage(r))))
            }
            seq++
        }
    }

    assert.deepStrictEqual(
        hexMessages,
        fixture.messages,
        "TS encoder hex output must match committed Rust fixture messages byte-for-byte",
    )
})
test("fec v2 source and repair vectors match the committed Rust fixture", () => {
    assert.deepEqual(
        fixture.v2_symbols.map(record => record.kind),
        ["source", "repair"],
        "fixture must include canonical source then repair v2 symbols",
    )

    for (const record of fixture.v2_symbols) {
        const payload = hexToU8(record.payload_hex)
        const symbol = record.kind === "source"
            ? {
                kind: "source",
                version: 2,
                epoch: record.epoch,
                seq: record.seq,
                payload,
            }
            : {
                kind: "repair",
                version: 2,
                epoch: record.epoch,
                repairSeq: record.repair_seq,
                windowBase: record.window_base,
                windowEnd: record.window_end,
                payload,
            }

        assert.equal(crc32(payload), record.payload_crc32, `${record.kind} payload CRC must match fixture`)
        const encoded = encodeSymbolMessageV2(symbol)
        const bytes = new Uint8Array(encoded)
        const view = new DataView(encoded)
        const crcOffset = record.kind === "source" ? 11 : 17
        assert.equal(u8ToHex(bytes), record.encoded_hex, `${record.kind} encoding must match Rust fixture`)
        assert.equal(view.getUint32(crcOffset, true), record.payload_crc32, `${record.kind} wire CRC must match fixture`)

        const decoded = parseSymbolMessage(encoded)
        assert.ok(decoded, `${record.kind} encoding must parse`)
        assert.equal(decoded.kind, record.kind)
        assert.equal(decoded.version, 2)
        assert.equal(decoded.epoch, record.epoch)
        assert.deepEqual(decoded.payload, payload)
        if (record.kind === "source") {
            assert.equal(decoded.seq, record.seq)
        } else {
            assert.equal(decoded.repairSeq, record.repair_seq)
            assert.equal(decoded.windowBase, record.window_base)
            assert.equal(decoded.windowEnd, record.window_end)
        }
    }
})
