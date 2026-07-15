// `cursor` DataChannel wire format — M4 cursor P1/P2
// (docs/design/cursor-channel.md §3, §P2).
//
// POS message, little-endian, 18 bytes (v2 — shape_id appended in P2):
//   u8 kind=0 | u8 visible(0/1) | i32 x | i32 y | u16 vw | u16 vh | u32 shape_id
//
// x,y are host cursor screen coordinates and vw,vh the captured monitor's
// size in the same coordinate space — the client maps the normalized
// position onto the video rect. shape_id is 0 for "unknown/none", otherwise
// refers to the last SHAPE message with that id.
//
// SHAPE message, little-endian, 17-byte header + PNG payload:
//   u8 kind=1 | u32 shape_id | u16 w | u16 h | u16 hot_x | u16 hot_y | u32 png_len | png bytes
// Sent only when the host cursor's shape changes; png_len (and the whole
// frame) is capped at CURSOR_SHAPE_MAX_PNG_LEN bytes.
//
// Mirror: streamer/src/transport/cursor_wire.rs — keep the byte-pinned
// tests in lockstep (qu_wire pattern). Unknown kinds are ignored here,
// matching the Rust side's forward-compat contract.

export const CURSOR_KIND_POS = 0
export const CURSOR_KIND_SHAPE = 1
export const CURSOR_POS_LEN = 18
export const CURSOR_SHAPE_HEADER_LEN = 17
export const CURSOR_SHAPE_MAX_PNG_LEN = 262144

export type CursorPosMessage = {
    kind: 'pos'
    visible: boolean
    x: number
    y: number
    vw: number
    vh: number
    shapeId: number
}

export type CursorShapeMessage = {
    kind: 'shape'
    shapeId: number
    w: number
    h: number
    hotX: number
    hotY: number
    png: Uint8Array
}

export type CursorMessage = CursorPosMessage | CursorShapeMessage

/** Parse one cursor channel message; null on malformed/unknown input. */
export function parseCursorMessage(buf: ArrayBuffer): CursorMessage | null {
    const view = new DataView(buf)
    if (view.byteLength < 1) {
        return null
    }
    const kind = view.getUint8(0)

    if (kind === CURSOR_KIND_POS) {
        if (view.byteLength < CURSOR_POS_LEN) {
            return null
        }
        return {
            kind: 'pos',
            visible: view.getUint8(1) !== 0,
            x: view.getInt32(2, true),
            y: view.getInt32(6, true),
            vw: view.getUint16(10, true),
            vh: view.getUint16(12, true),
            shapeId: view.getUint32(14, true),
        }
    }

    if (kind === CURSOR_KIND_SHAPE) {
        if (view.byteLength < CURSOR_SHAPE_HEADER_LEN) {
            return null
        }
        const shapeId = view.getUint32(1, true)
        const w = view.getUint16(5, true)
        const h = view.getUint16(7, true)
        const hotX = view.getUint16(9, true)
        const hotY = view.getUint16(11, true)
        const pngLen = view.getUint32(13, true)
        if (pngLen > CURSOR_SHAPE_MAX_PNG_LEN) {
            return null
        }
        if (view.byteLength < CURSOR_SHAPE_HEADER_LEN + pngLen) {
            return null
        }
        const png = new Uint8Array(buf, CURSOR_SHAPE_HEADER_LEN, pngLen)
        return { kind: 'shape', shapeId, w, h, hotX, hotY, png }
    }

    // Unknown kind — forward-compat: ignore rather than throw.
    return null
}

/**
 * Converts raw bytes to a base64 string without `String.fromCharCode(...spread)`
 * on the whole buffer, which blows the call-stack argument limit on large
 * (e.g. ~100 KB PNG) inputs. Chunked at 32 KB, a conventional safe size for
 * this pattern.
 */
export function bytesToBase64(bytes: Uint8Array): string {
    const CHUNK_SIZE = 0x8000
    let binary = ''
    for (let i = 0; i < bytes.length; i += CHUNK_SIZE) {
        const chunk = bytes.subarray(i, i + CHUNK_SIZE)
        binary += String.fromCharCode(...chunk)
    }
    return btoa(binary)
}
