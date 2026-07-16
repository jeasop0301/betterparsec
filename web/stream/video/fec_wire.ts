// Wire-format parse / serialise for the video_fec DataChannel (U2).
// All integers and CRCs are little-endian. V1 is retained for degraded peers.

import type { FecSymbol } from "./fec.js";

export const V1_SOURCE_KIND = 0x00;
export const V1_REPAIR_KIND = 0x01;
export const V2_SOURCE_KIND = 0x02;
export const V2_REPAIR_KIND = 0x03;
export const NEEDS_IDR_KIND = 0x80;
export const ACK_KIND = 0x81;
export const SUBSCRIBE_KIND = 0x82;

export const SOURCE_V2_HEADER_SIZE = 15;
export const REPAIR_V2_HEADER_SIZE = 21;
export const CHUNK_HEADER_SIZE = 13;
export const CHUNK_V2_HEADER_SIZE = 21;
export const SOURCE_MESSAGE_MAX = 1200;
export const SOURCE_PAYLOAD_MAX = 1185;
export const REPAIR_PAYLOAD_MAX = 1200;
export const REPAIR_MESSAGE_MAX = 1221;
export const CHUNK_FRAGMENT_MAX = 1182;
export const CHUNK_V2_FRAGMENT_MAX = 1164;
export const FRAME_MAX_BYTES = 4 * 1024 * 1024;
export const CHUNK_COUNT_MAX = 4096;

export type WireVersion = 1 | 2;
export type V1FecSymbol = FecSymbol & { version: 1; epoch?: never };
export type V2FecSymbol = FecSymbol & { version: 2; epoch: number };
export type VersionedFecSymbol = V1FecSymbol | V2FecSymbol;
export interface ChunkHeader {
    frameId: number;
    chunkIndex: number;
    chunkCount: number;
    frameType: 0 | 1;
    timestampUs: number;
}
export interface ChunkHeaderV2 extends ChunkHeader {
    encodedFrameLen: number;
    encodedFrameCrc32: number;
}
export interface AckControl { version: 2; epoch: number; highestSeq: number; }
export interface SubscribeControl { version: 2; epoch: number; }
export interface NeedsIdrControl { version: 2; epoch: number; reason: number; }

/** Legacy controls for v1 peers. */
export const SUBSCRIBE_MESSAGE: ArrayBuffer = new Uint8Array([0x01]).buffer;
export const NEEDS_IDR_MESSAGE: ArrayBuffer = new Uint8Array([0x00]).buffer;

function crc32(bytes: Uint8Array): number {
    let crc = 0xFFFF_FFFF;
    for (const byte of bytes) {
        crc ^= byte;
        for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (crc & 1 ? 0xEDB8_8320 : 0);
    }
    return (crc ^ 0xFFFF_FFFF) >>> 0;
}
export { crc32 };

function nonZeroEpoch(epoch: number | undefined): epoch is number {
    return epoch !== undefined && Number.isInteger(epoch) && epoch > 0 && epoch <= 0xFFFF_FFFF;
}
function validChunk(frameType: number, chunkIndex: number, chunkCount: number, frameLen: number, fragmentLength: number): boolean {
    const expectedCount = Math.max(1, Math.ceil(frameLen / CHUNK_V2_FRAGMENT_MAX));
    const expectedFragmentLength = chunkIndex + 1 === chunkCount
        ? frameLen - CHUNK_V2_FRAGMENT_MAX * (chunkCount - 1)
        : CHUNK_V2_FRAGMENT_MAX;
    return (frameType === 0 || frameType === 1) && chunkCount >= 1 && chunkCount <= CHUNK_COUNT_MAX &&
        chunkIndex < chunkCount && frameLen <= FRAME_MAX_BYTES && chunkCount === expectedCount &&
        fragmentLength === expectedFragmentLength;
}

/** Parses either unambiguous v1 symbols or fail-closed v2 symbols. */
export function parseSymbolMessage(buf: ArrayBuffer): VersionedFecSymbol | null {
    const bytes = new Uint8Array(buf);
    if (bytes.length < 1) return null;
    const view = new DataView(buf);
    if (bytes[0] === V1_SOURCE_KIND) {
        if (bytes.length < 5) return null;
        return { kind: "source", seq: view.getUint32(1, true), payload: bytes.slice(5), version: 1 };
    }
    if (bytes[0] === V1_REPAIR_KIND) {
        if (bytes.length < 11) return null;
        return { kind: "repair", repairSeq: view.getUint16(1, true), windowBase: view.getUint32(3, true), windowEnd: view.getUint32(7, true), payload: bytes.slice(11), version: 1 };
    }
    if (bytes[0] === V2_SOURCE_KIND) {
        if (bytes.length < SOURCE_V2_HEADER_SIZE || bytes.length > SOURCE_MESSAGE_MAX) return null;

        const epoch = view.getUint32(1, true);
        const seq = view.getUint32(5, true);
        const length = view.getUint16(9, true);
        const payload = bytes.subarray(SOURCE_V2_HEADER_SIZE);
        const payloadCrc32 = view.getUint32(11, true);

        if (!nonZeroEpoch(epoch) ||
            length > SOURCE_PAYLOAD_MAX ||
            bytes.length !== SOURCE_V2_HEADER_SIZE + length ||
            crc32(payload) !== payloadCrc32 ||
            !parseChunkV2Bytes(payload)) return null;

        return { kind: "source", seq, payload: payload.slice(), version: 2, epoch };
    }
    if (bytes[0] === V2_REPAIR_KIND) {
        if (bytes.length < REPAIR_V2_HEADER_SIZE || bytes.length > REPAIR_MESSAGE_MAX) return null;

        const epoch = view.getUint32(1, true);
        const repairSeq = view.getUint16(5, true);
        const windowBase = view.getUint32(7, true);
        const windowEnd = view.getUint32(11, true);
        const length = view.getUint16(15, true);
        const payload = bytes.subarray(REPAIR_V2_HEADER_SIZE);
        const payloadCrc32 = view.getUint32(17, true);

        if (!nonZeroEpoch(epoch) ||
            length > REPAIR_PAYLOAD_MAX ||
            bytes.length !== REPAIR_V2_HEADER_SIZE + length ||
            crc32(payload) !== payloadCrc32) return null;

        return { kind: "repair", repairSeq, windowBase, windowEnd, payload: payload.slice(), version: 2, epoch };
    }
    return null;
}

/** Encodes a bare FEC symbol as v1 or an explicitly versioned symbol as that version. */
export function encodeSymbolMessage(sym: FecSymbol | VersionedFecSymbol): ArrayBuffer {
    const versioned = sym as { version?: unknown; epoch?: unknown };
    if (versioned.version === 2) return encodeSymbolMessageV2(sym as V2FecSymbol);
    if (versioned.version !== 1 && "version" in versioned) {
        throw new RangeError("unsupported FEC wire version");
    }
    if ("epoch" in versioned) throw new RangeError("v1 symbols must not include an epoch");

    if (sym.kind === "source") {
        const buf = new ArrayBuffer(5 + sym.payload.length);
        const view = new DataView(buf);
        view.setUint8(0, V1_SOURCE_KIND);
        view.setUint32(1, sym.seq >>> 0, true);
        new Uint8Array(buf).set(sym.payload, 5);
        return buf;
    }

    const buf = new ArrayBuffer(11 + sym.payload.length);
    const view = new DataView(buf);
    view.setUint8(0, V1_REPAIR_KIND);
    view.setUint16(1, sym.repairSeq & 0xFFFF, true);
    view.setUint32(3, sym.windowBase >>> 0, true);
    view.setUint32(7, sym.windowEnd >>> 0, true);
    new Uint8Array(buf).set(sym.payload, 11);
    return buf;
}

export function encodeSymbolMessageV2(sym: V2FecSymbol): ArrayBuffer {
    if (sym.version !== 2 || !nonZeroEpoch(sym.epoch)) throw new RangeError("v2 symbols require a nonzero epoch");

    if (sym.kind === "source") {
        if (sym.payload.length > SOURCE_PAYLOAD_MAX) {
            throw new RangeError("v2 symbol payload exceeds wire cap");
        }
        if (!parseChunkV2Bytes(sym.payload)) {
            throw new RangeError("v2 source payload must be a valid chunk");
        }

        const buf = new ArrayBuffer(SOURCE_V2_HEADER_SIZE + sym.payload.length);
        const view = new DataView(buf);
        const bytes = new Uint8Array(buf);
        view.setUint8(0, V2_SOURCE_KIND);
        view.setUint32(1, sym.epoch, true);
        view.setUint32(5, sym.seq >>> 0, true);
        view.setUint16(9, sym.payload.length, true);
        view.setUint32(11, crc32(sym.payload), true);
        bytes.set(sym.payload, SOURCE_V2_HEADER_SIZE);
        return buf;
    }

    if (sym.payload.length > REPAIR_PAYLOAD_MAX) {
        throw new RangeError("v2 symbol payload exceeds wire cap");
    }

    const buf = new ArrayBuffer(REPAIR_V2_HEADER_SIZE + sym.payload.length);
    const view = new DataView(buf);
    const bytes = new Uint8Array(buf);
    view.setUint8(0, V2_REPAIR_KIND);
    view.setUint32(1, sym.epoch, true);
    view.setUint16(5, sym.repairSeq & 0xFFFF, true);
    view.setUint32(7, sym.windowBase >>> 0, true);
    view.setUint32(11, sym.windowEnd >>> 0, true);
    view.setUint16(15, sym.payload.length, true);
    view.setUint32(17, crc32(sym.payload), true);
    bytes.set(sym.payload, REPAIR_V2_HEADER_SIZE);
    return buf;
}

export function parseChunkHeader(buf: ArrayBuffer): ChunkHeader | null {
    if (buf.byteLength < CHUNK_HEADER_SIZE) return null;
    const view = new DataView(buf), frameType = view.getUint8(8), chunkIndex = view.getUint16(4, true), chunkCount = view.getUint16(6, true);
    return { frameId: view.getUint32(0, true), chunkIndex, chunkCount, frameType: frameType as 0 | 1, timestampUs: view.getUint32(9, true) };
}

/** Parses an exact v2 chunk after structural cap and length validation. The frame CRC is verified after reassembly. */
export function parseChunkHeaderV2(buf: ArrayBuffer): ChunkHeaderV2 | null {
    return parseChunkV2Bytes(new Uint8Array(buf));
}
function parseChunkV2Bytes(bytes: Uint8Array): ChunkHeaderV2 | null {
    if (bytes.length < CHUNK_V2_HEADER_SIZE || bytes.length > SOURCE_PAYLOAD_MAX) return null;

    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const chunkIndex = view.getUint16(4, true);
    const chunkCount = view.getUint16(6, true);
    const frameType = view.getUint8(8);
    const frameLen = view.getUint32(13, true);
    const frameCrc = view.getUint32(17, true);
    const fragment = bytes.subarray(CHUNK_V2_HEADER_SIZE);

    if (!validChunk(frameType, chunkIndex, chunkCount, frameLen, fragment.length)) return null;

    return {
        frameId: view.getUint32(0, true),
        chunkIndex,
        chunkCount,
        frameType: frameType as 0 | 1,
        timestampUs: view.getUint32(9, true),
        encodedFrameLen: frameLen,
        encodedFrameCrc32: frameCrc,
    };
}

/** Validates v2 frame bytes only after all chunks have been reassembled. */
export function verifyEncodedFrameV2(header: ChunkHeaderV2, frame: Uint8Array): boolean {
    return frame.length === header.encodedFrameLen && crc32(frame) === header.encodedFrameCrc32;
}

export function chunkFrame(frameId: number, isKey: boolean, timestampUs: number, data: Uint8Array): ArrayBuffer[] {
    const frameType = isKey ? 1 : 0;
    const count = Math.max(1, Math.ceil(data.length / CHUNK_FRAGMENT_MAX));
    const chunks: ArrayBuffer[] = [];
    for (let i = 0; i < count; i++) { const fragment = data.slice(i * CHUNK_FRAGMENT_MAX, Math.min((i + 1) * CHUNK_FRAGMENT_MAX, data.length)); const buf = new ArrayBuffer(CHUNK_HEADER_SIZE + fragment.length), view = new DataView(buf); view.setUint32(0, frameId >>> 0, true); view.setUint16(4, i, true); view.setUint16(6, count, true); view.setUint8(8, frameType); view.setUint32(9, timestampUs >>> 0, true); new Uint8Array(buf).set(fragment, CHUNK_HEADER_SIZE); chunks.push(buf); }
    return chunks;
}

export function chunkFrameV2(frameId: number, isKey: boolean, timestampUs: number, data: Uint8Array): ArrayBuffer[] {
    if (data.length > FRAME_MAX_BYTES) throw new RangeError("frame exceeds v2 wire cap");

    const frameType = isKey ? 1 : 0;
    const count = Math.max(1, Math.ceil(data.length / CHUNK_V2_FRAGMENT_MAX));
    if (count > CHUNK_COUNT_MAX) throw new RangeError("frame requires too many chunks");

    const frameCrc = crc32(data);
    const chunks: ArrayBuffer[] = [];
    for (let i = 0; i < count; i++) {
        const fragmentStart = i * CHUNK_V2_FRAGMENT_MAX;
        const fragmentEnd = Math.min(fragmentStart + CHUNK_V2_FRAGMENT_MAX, data.length);
        const fragment = data.slice(fragmentStart, fragmentEnd);
        const buf = new ArrayBuffer(CHUNK_V2_HEADER_SIZE + fragment.length);
        const view = new DataView(buf);

        view.setUint32(0, frameId >>> 0, true);
        view.setUint16(4, i, true);
        view.setUint16(6, count, true);
        view.setUint8(8, frameType);
        view.setUint32(9, timestampUs >>> 0, true);
        view.setUint32(13, data.length, true);
        view.setUint32(17, frameCrc, true);
        new Uint8Array(buf).set(fragment, CHUNK_V2_HEADER_SIZE);
        chunks.push(buf);
    }
    return chunks;
}

export function encodeAck(highest: number, epoch?: number): ArrayBuffer {
    if (epoch !== undefined) return encodeAckV2(epoch, highest);
    const buf = new ArrayBuffer(4);
    new DataView(buf).setUint32(0, highest >>> 0, true);
    return buf;
}
export function encodeAckV2(epoch: number, highestSeq: number): ArrayBuffer { if (!nonZeroEpoch(epoch)) throw new RangeError("v2 controls require a nonzero epoch"); const buf = new ArrayBuffer(9), view = new DataView(buf); view.setUint8(0, ACK_KIND); view.setUint32(1, epoch, true); view.setUint32(5, highestSeq >>> 0, true); return buf; }
export function encodeSubscribeV2(epoch: number): ArrayBuffer { if (!nonZeroEpoch(epoch)) throw new RangeError("v2 controls require a nonzero epoch"); const buf = new ArrayBuffer(5), view = new DataView(buf); view.setUint8(0, SUBSCRIBE_KIND); view.setUint32(1, epoch, true); return buf; }
export function encodeNeedsIdrV2(epoch: number, reason: number): ArrayBuffer { if (!nonZeroEpoch(epoch) || !Number.isInteger(reason) || reason < 0 || reason > 255) throw new RangeError("invalid v2 NeedsIdr control"); const buf = new ArrayBuffer(6), view = new DataView(buf); view.setUint8(0, NEEDS_IDR_KIND); view.setUint32(1, epoch, true); view.setUint8(5, reason); return buf; }
export function parseControlMessage(buf: ArrayBuffer): AckControl | SubscribeControl | NeedsIdrControl | null { const bytes = new Uint8Array(buf); if (bytes.length < 5) return null; const view = new DataView(buf), epoch = view.getUint32(1, true); if (!nonZeroEpoch(epoch)) return null; if (bytes[0] === SUBSCRIBE_KIND && bytes.length === 5) return { version: 2, epoch }; if (bytes[0] === ACK_KIND && bytes.length === 9) return { version: 2, epoch, highestSeq: view.getUint32(5, true) }; if (bytes[0] === NEEDS_IDR_KIND && bytes.length === 6) return { version: 2, epoch, reason: view.getUint8(5) }; return null; }
