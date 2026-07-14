// Wire-format parse / serialise for the video_fec DataChannel (U2).
// All integers are little-endian per docs/design/fec-framing.md §2.

import type { FecSymbol } from "./fec.js";

// ── Constants ─────────────────────────────────────────────────────────────

/** Chunk header size in bytes (frame_id u32 + chunk_index u16 + chunk_count u16 + frame_type u8 + timestamp_us u32). */
export const CHUNK_HEADER_SIZE = 13;

/**
 * Maximum Annex-B fragment bytes per source symbol.
 * Source message = 1 (kind) + 4 (seq) + CHUNK_HEADER_SIZE + fragment <= 1200 B.
 * 1200 - 5 - 13 = 1182.
 */
export const CHUNK_FRAGMENT_MAX = 1182;

/**
 * SUBSCRIBE message: 1-byte 0x01 sent on video_fec_ack to activate the host sender.
 * Constructed as a frozen ArrayBuffer containing [0x01].
 */
export const SUBSCRIBE_MESSAGE: ArrayBuffer = new Uint8Array([0x01]).buffer;

/**
 * NEEDS_IDR message: 1-byte 0x00 sent on video_fec_ack to request a keyframe
 * after unrecoverable loss.
 */
export const NEEDS_IDR_MESSAGE: ArrayBuffer = new Uint8Array([0x00]).buffer;

// ── Symbol message wire format ────────────────────────────────────────────

/**
 * Parse a binary video_fec DataChannel message into a FecSymbol.
 * Source symbol: [u8 kind=0][u32 LE seq][...chunk payload]
 * Repair symbol: [u8 kind=1][u16 LE repair_seq][u32 LE window_base][u32 LE window_end][...payload]
 * Returns null if the buffer is too short or kind is unknown.
 */
export function parseSymbolMessage(buf: ArrayBuffer): FecSymbol | null {
    const view = new DataView(buf);
    const bytes = new Uint8Array(buf);
    if (bytes.length < 1) return null;
    const kind = view.getUint8(0);
    if (kind === 0) {
        if (bytes.length < 5) return null;
        const seq = view.getUint32(1, true);
        const payload = bytes.slice(5);
        return { kind: 'source', seq, payload };
    } else if (kind === 1) {
        if (bytes.length < 11) return null;
        const repairSeq = view.getUint16(1, true);
        const windowBase = view.getUint32(3, true);
        const windowEnd = view.getUint32(7, true);
        const payload = bytes.slice(11);
        return { kind: 'repair', repairSeq, windowBase, windowEnd, payload };
    }
    return null;
}

/**
 * Serialise a FecSymbol to a binary video_fec DataChannel message.
 */
export function encodeSymbolMessage(sym: FecSymbol): ArrayBuffer {
    if (sym.kind === 'source') {
        const buf = new ArrayBuffer(5 + sym.payload.length);
        const view = new DataView(buf);
        const bytes = new Uint8Array(buf);
        view.setUint8(0, 0);
        view.setUint32(1, sym.seq >>> 0, true);
        bytes.set(sym.payload, 5);
        return buf;
    } else {
        const buf = new ArrayBuffer(11 + sym.payload.length);
        const view = new DataView(buf);
        const bytes = new Uint8Array(buf);
        view.setUint8(0, 1);
        view.setUint16(1, sym.repairSeq & 0xFFFF, true);
        view.setUint32(3, sym.windowBase >>> 0, true);
        view.setUint32(7, sym.windowEnd >>> 0, true);
        bytes.set(sym.payload, 11);
        return buf;
    }
}

// ── Chunk layer ───────────────────────────────────────────────────────────

export interface ChunkHeader {
    frameId: number;      // u32
    chunkIndex: number;   // u16
    chunkCount: number;   // u16
    frameType: 0 | 1;     // 0=delta, 1=key
    timestampUs: number;  // u32
}

/**
 * Parse the 13-byte chunk header from a source-symbol payload.
 * Returns null if the buffer is shorter than CHUNK_HEADER_SIZE.
 */
export function parseChunkHeader(buf: ArrayBuffer): ChunkHeader | null {
    if (buf.byteLength < CHUNK_HEADER_SIZE) return null;
    const view = new DataView(buf);
    return {
        frameId: view.getUint32(0, true),
        chunkIndex: view.getUint16(4, true),
        chunkCount: view.getUint16(6, true),
        frameType: view.getUint8(8) as 0 | 1,
        timestampUs: view.getUint32(9, true),
    };
}

/**
 * Chunk a video frame into one or more source-symbol payloads.
 * Each chunk is a CHUNK_HEADER_SIZE-byte header followed by up to CHUNK_FRAGMENT_MAX bytes of Annex-B.
 * Empty frame data produces exactly one chunk with an empty fragment.
 */
export function chunkFrame(
    frameId: number,
    isKey: boolean,
    timestampUs: number,
    data: Uint8Array,
): ArrayBuffer[] {
    const frameType = isKey ? 1 : 0;
    const chunks: ArrayBuffer[] = [];

    if (data.length === 0) {
        const buf = new ArrayBuffer(CHUNK_HEADER_SIZE);
        writeChunkHeader(new DataView(buf), frameId, 0, 1, frameType, timestampUs);
        chunks.push(buf);
        return chunks;
    }

    const chunkCount = Math.ceil(data.length / CHUNK_FRAGMENT_MAX);
    for (let i = 0; i < chunkCount; i++) {
        const start = i * CHUNK_FRAGMENT_MAX;
        const end = Math.min(start + CHUNK_FRAGMENT_MAX, data.length);
        const fragment = data.slice(start, end);
        const buf = new ArrayBuffer(CHUNK_HEADER_SIZE + fragment.length);
        const view = new DataView(buf);
        writeChunkHeader(view, frameId, i, chunkCount, frameType, timestampUs);
        new Uint8Array(buf).set(fragment, CHUNK_HEADER_SIZE);
        chunks.push(buf);
    }
    return chunks;
}

function writeChunkHeader(
    view: DataView,
    frameId: number, chunkIndex: number, chunkCount: number,
    frameType: number, timestampUs: number,
): void {
    view.setUint32(0, frameId >>> 0, true);
    view.setUint16(4, chunkIndex & 0xFFFF, true);
    view.setUint16(6, chunkCount & 0xFFFF, true);
    view.setUint8(8, frameType & 0xFF);
    view.setUint32(9, timestampUs >>> 0, true);
}

// ── ACK wire format ───────────────────────────────────────────────────────

/**
 * Encode an ACK as a 4-byte little-endian u32 ArrayBuffer.
 * Sent on video_fec_ack to feed FecEncoder::acknowledge on the host.
 */
export function encodeAck(highest: number): ArrayBuffer {
    const buf = new ArrayBuffer(4);
    new DataView(buf).setUint32(0, highest >>> 0, true);
    return buf;
}
