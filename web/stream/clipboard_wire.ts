// Clipboard sync v1 (text-only) wire format.
//
// TEXT message, little-endian:
//   u8 kind=0 | u32 len | utf8 bytes
//
// Mirror: streamer/src/transport/clipboard.rs `encode_text` / `decode_text`
// — keep the byte-pinned tests in lockstep (cursor_wire pattern).

export const CLIPBOARD_KIND_TEXT = 0
// 256 KiB — generous for pasted text/URLs/code, small enough that a runaway
// clipboard cannot flood either transport.
export const CLIPBOARD_MAX_LEN = 262144
// Fixed header size: 1 kind byte + 4 length bytes.
const HEADER_LEN = 5

/**
 * Encodes `text` as a TEXT wire message.
 *
 * Returns `null` (instead of throwing) when the UTF-8 encoding of `text`
 * exceeds `CLIPBOARD_MAX_LEN` — the caller must skip the send and
 * debug-log instead of shipping a frame the peer's `decodeClipboardText`
 * would reject.
 */
export function encodeClipboardText(text: string): ArrayBuffer | null {
    const bytes = new TextEncoder().encode(text)
    if (bytes.length > CLIPBOARD_MAX_LEN) {
        return null
    }

    const out = new Uint8Array(HEADER_LEN + bytes.length)
    const view = new DataView(out.buffer)
    out[0] = CLIPBOARD_KIND_TEXT
    view.setUint32(1, bytes.length, true)
    out.set(bytes, HEADER_LEN)
    return out.buffer
}

/**
 * Parses one clipboard channel message; `null` on truncated/malformed
 * input, an oversized declared length, or an unknown `kind` byte (unknown
 * kinds are ignored rather than treated as an error — forward compatible
 * with a future non-text kind).
 */
export function decodeClipboardText(buf: ArrayBuffer): string | null {
    const view = new DataView(buf)
    if (view.byteLength < HEADER_LEN) {
        return null
    }
    if (view.getUint8(0) !== CLIPBOARD_KIND_TEXT) {
        return null
    }

    const len = view.getUint32(1, true)
    if (len > CLIPBOARD_MAX_LEN) {
        return null
    }
    if (view.byteLength < HEADER_LEN + len) {
        return null
    }

    const bytes = new Uint8Array(buf, HEADER_LEN, len)
    return new TextDecoder("utf-8").decode(bytes)
}
