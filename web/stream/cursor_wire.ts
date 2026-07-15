// `cursor` DataChannel wire format — M4 cursor P1
// (docs/design/cursor-channel.md §3).
//
// POS message, little-endian, 14 bytes:
//   u8 kind=0 | u8 visible(0/1) | i32 x | i32 y | u16 vw | u16 vh
//
// x,y are host cursor screen coordinates and vw,vh the captured monitor's
// size in the same coordinate space — the client maps the normalized
// position onto the video rect.
//
// Mirror: streamer/src/transport/webrtc/cursor_wire.rs — keep the
// byte-pinned tests in lockstep (qu_wire pattern).

export const CURSOR_KIND_POS = 0
export const CURSOR_POS_LEN = 14

export type CursorPosMessage = {
    kind: 'pos'
    visible: boolean
    x: number
    y: number
    vw: number
    vh: number
}

export type CursorMessage = CursorPosMessage

/** Parse one cursor channel message; null on malformed/unknown input. */
export function parseCursorMessage(buf: ArrayBuffer): CursorMessage | null {
    const view = new DataView(buf)
    if (view.byteLength < 1) {
        return null
    }
    if (view.getUint8(0) !== CURSOR_KIND_POS) {
        return null
    }
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
    }
}
