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
    // Reject invalid UTF-8, matching the Rust peer's String::from_utf8().ok()
    // (streamer/src/transport/clipboard.rs) so both languages accept/reject the
    // same frames — a non-fatal decoder would silently accept with U+FFFD.
    try {
        return new TextDecoder("utf-8", { fatal: true }).decode(bytes)
    } catch {
        return null
    }
}

// Image (PNG) wire — mirror of streamer/src/transport/clipboard.rs `image`.
//   u8 kind=1 (IMAGE) | u16 width | u16 height | u32 png_len | png bytes

export const CLIPBOARD_KIND_IMAGE = 1
// 8 MiB — a full-screen PNG screenshot fits; bounds a runaway image paste.
export const CLIPBOARD_IMAGE_MAX_LEN = 8 * 1024 * 1024
const IMAGE_HEADER_LEN = 9
const PNG_SIGNATURE = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]

export type ClipboardImage = { width: number; height: number; png: Uint8Array }

function startsWithPngSignature(bytes: Uint8Array): boolean {
    if (bytes.length < PNG_SIGNATURE.length) return false
    for (let i = 0; i < PNG_SIGNATURE.length; i++) {
        if (bytes[i] !== PNG_SIGNATURE[i]) return false
    }
    return true
}

/** Encodes a PNG image; null if the payload is not a PNG or exceeds the cap. */
export function encodeClipboardImage(width: number, height: number, png: Uint8Array): ArrayBuffer | null {
    if (png.length > CLIPBOARD_IMAGE_MAX_LEN || !startsWithPngSignature(png)) {
        return null
    }
    const out = new Uint8Array(IMAGE_HEADER_LEN + png.length)
    const view = new DataView(out.buffer)
    out[0] = CLIPBOARD_KIND_IMAGE
    view.setUint16(1, width & 0xffff, true)
    view.setUint16(3, height & 0xffff, true)
    view.setUint32(5, png.length, true)
    out.set(png, IMAGE_HEADER_LEN)
    return out.buffer
}

/** Parses an IMAGE frame; null on truncation, oversize, wrong kind, or non-PNG. */
export function decodeClipboardImage(buf: ArrayBuffer): ClipboardImage | null {
    const view = new DataView(buf)
    if (view.byteLength < IMAGE_HEADER_LEN) return null
    if (view.getUint8(0) !== CLIPBOARD_KIND_IMAGE) return null
    const width = view.getUint16(1, true)
    const height = view.getUint16(3, true)
    const len = view.getUint32(5, true)
    if (len > CLIPBOARD_IMAGE_MAX_LEN) return null
    if (view.byteLength < IMAGE_HEADER_LEN + len) return null
    const png = new Uint8Array(buf.slice(IMAGE_HEADER_LEN, IMAGE_HEADER_LEN + len))
    if (!startsWithPngSignature(png)) return null
    return { width, height, png }
}
