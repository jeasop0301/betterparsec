// Standard CRC-32 (IEEE 802.3) implementation.
// Polynomial: 0xEDB88320 (reversed 0x04C11DB7), init 0xFFFFFFFF, final XOR 0xFFFFFFFF.
// Known check vector: crc32("123456789" ascii) === 0xCBF43926.
// Used by qu_overlay.ts to verify QU_TILE BGRA pixel data after PNG decode.

const TABLE: Uint32Array = (() => {
    const t = new Uint32Array(256)
    for (let i = 0; i < 256; i++) {
        let c = i
        for (let k = 0; k < 8; k++) {
            c = (c & 1) ? (0xEDB88320 ^ (c >>> 1)) : (c >>> 1)
        }
        t[i] = c >>> 0
    }
    return t
})()

/**
 * Compute CRC-32 (IEEE 802.3) over `data`.
 * Returns the 32-bit result as an unsigned integer.
 */
export function crc32(data: Uint8Array): number {
    let crc = 0xFFFFFFFF
    for (let i = 0; i < data.length; i++) {
        crc = TABLE[(crc ^ data[i]) & 0xFF] ^ (crc >>> 8)
    }
    return (crc ^ 0xFFFFFFFF) >>> 0
}
