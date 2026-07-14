// Wire-format parse / encode for the video_qu DataChannel (U4 P1).
// All integers are LITTLE-ENDIAN per docs/design/qu-protocol.md §2.
// host→client kinds: 0x01–0x04; client→host kinds: 0x81–0x82.

// ── Message types ─────────────────────────────────────────────────────────

export interface QuConfigMsg {
    kind: 0x01
    tileW: number    // u16 LE
    tileH: number    // u16 LE
    gridCols: number // u16 LE
    gridRows: number // u16 LE
    epoch: number    // u32 LE
}

export interface QuTileMsg {
    kind: 0x02
    epoch: number      // u32 LE
    col: number        // u16 LE
    row: number        // u16 LE
    format: number     // u8  (0=PNG)
    flags: number      // u8
    crc32Bgra: number  // u32 LE  — CRC32 over BGRA of decoded pixels
    payload: Uint8Array
}

export interface QuInvalidateMsg {
    kind: 0x03
    epoch: number
    tiles: Array<{ col: number; row: number }>
}

export interface QuEpochMsg {
    kind: 0x04
    epoch: number // u32 LE new_epoch
}

export interface QuSubscribeMsg {
    kind: 0x81
    version: number // u8; must be 1
}

export interface QuBudgetMsg {
    kind: 0x82
    kbps: number // u32 LE; 0 = pause
}

export type QuMessage =
    | QuConfigMsg
    | QuTileMsg
    | QuInvalidateMsg
    | QuEpochMsg
    | QuSubscribeMsg
    | QuBudgetMsg

// ── Parse ─────────────────────────────────────────────────────────────────

/**
 * Parse one video_qu DataChannel message from a raw ArrayBuffer.
 * Returns null on any malformed condition (truncated, payload_len mismatch,
 * count mismatch, unknown kind).
 */
export function parseQuMessage(buf: ArrayBuffer): QuMessage | null {
    const bytes = new Uint8Array(buf)
    if (bytes.length < 1) return null
    const view = new DataView(buf)
    const kind = view.getUint8(0)

    switch (kind) {
        case 0x01: { // QU_CONFIG: 1+2+2+2+2+4 = 13 bytes
            if (bytes.length < 13) return null
            return {
                kind: 0x01,
                tileW: view.getUint16(1, true),
                tileH: view.getUint16(3, true),
                gridCols: view.getUint16(5, true),
                gridRows: view.getUint16(7, true),
                epoch: view.getUint32(9, true),
            }
        }
        case 0x02: { // QU_TILE: 1+4+2+2+1+1+4+4 = 19 header bytes + payload
            if (bytes.length < 19) return null
            const payloadLen = view.getUint32(15, true)
            if (bytes.length < 19 + payloadLen) return null
            // Accept trailing bytes after the declared payload_len (alignment
            // padding or future extension fields) to match the Rust parser
            // which only requires buf.len() >= TILE_HDR + payload_len.
            return {
                kind: 0x02,
                epoch: view.getUint32(1, true),
                col: view.getUint16(5, true),
                row: view.getUint16(7, true),
                format: view.getUint8(9),
                flags: view.getUint8(10),
                crc32Bgra: view.getUint32(11, true),
                payload: bytes.slice(19, 19 + payloadLen),
            }
        }
        case 0x03: { // QU_INVALIDATE: 1+4+2 = 7 header bytes + count*(2+2)
            if (bytes.length < 7) return null
            const count = view.getUint16(5, true)
            if (bytes.length !== 7 + count * 4) return null
            const tiles: Array<{ col: number; row: number }> = []
            let off = 7
            for (let i = 0; i < count; i++) {
                tiles.push({
                    col: view.getUint16(off, true),
                    row: view.getUint16(off + 2, true),
                })
                off += 4
            }
            return { kind: 0x03, epoch: view.getUint32(1, true), tiles }
        }
        case 0x04: { // QU_EPOCH: 1+4 = 5 bytes
            if (bytes.length < 5) return null
            return { kind: 0x04, epoch: view.getUint32(1, true) }
        }
        case 0x81: { // QU_SUBSCRIBE: 1+1 = 2 bytes
            if (bytes.length < 2) return null
            return { kind: 0x81, version: view.getUint8(1) }
        }
        case 0x82: { // QU_BUDGET: 1+4 = 5 bytes
            if (bytes.length < 5) return null
            return { kind: 0x82, kbps: view.getUint32(1, true) }
        }
        default:
            return null
    }
}

// ── Client→host encoders ──────────────────────────────────────────────────

/**
 * Encode QU_SUBSCRIBE (version=1): [0x81, 0x01].
 * Send on channel open to activate the host QU sender.
 */
export function encodeSubscribe(): ArrayBuffer {
    return new Uint8Array([0x81, 0x01]).buffer
}

/**
 * Encode QU_BUDGET: [0x82, kbps u32 LE].
 * Pass kbps=0 to pause tile delivery.
 */
export function encodeBudget(kbps: number): ArrayBuffer {
    const buf = new ArrayBuffer(5)
    const view = new DataView(buf)
    view.setUint8(0, 0x82)
    view.setUint32(1, kbps >>> 0, true)
    return buf
}
