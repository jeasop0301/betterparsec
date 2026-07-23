// Media color / display capability schema (G017) — TS mirror of
// common/src/media_caps.rs. Describes a full-fidelity video mode and reconciles
// a requested mode against both peers' capabilities into a supported mode plus a
// typed downgrade reason (the downgrade matrix), with a deterministic SDR
// fallback when HDR is not mutually supported. Keep the truth vectors in
// tests/media_caps.test.mjs in lockstep with the Rust `tests`.

export type VideoCodec = "h264" | "hevc" | "av1"
export type ChromaSubsampling = "yuv420" | "yuv444"
export type BitDepth = 8 | 10
export type ColorPrimaries = "bt709" | "bt2020"
export type TransferFunction = "bt709" | "pq" | "hlg"
export type MatrixCoefficients = "bt709" | "bt2020ncl"
export type ColorRange = "limited" | "full"

export type HdrMetadata = {
    maxCll: number
    maxFall: number
    maxLuminance: number
    minLuminance: number
}

export type VideoMode = {
    codec: VideoCodec
    chroma: ChromaSubsampling
    bitDepth: BitDepth
    primaries: ColorPrimaries
    transfer: TransferFunction
    matrix: MatrixCoefficients
    range: ColorRange
    hdr: HdrMetadata | null
    width: number
    height: number
    refreshMhz: number
    vrr: boolean
}

export type Capabilities = {
    codecs: number
    chroma444: boolean
    bit10: boolean
    hdr: boolean
    maxRefreshMhz: number
    vrr: boolean
}

// Typed downgrade reason names (mirror of the G009 DowngradeReason variants used
// by negotiation).
export type DowngradeReasonName = "unsupported" | "capability_mismatch"

export const CODEC_BIT: Readonly<Record<VideoCodec, number>> = { h264: 0x01, hevc: 0x02, av1: 0x04 }

/** An always-supported 1080p60 SDR H.264 4:2:0 8-bit floor mode. */
export function sdrBaseline(): VideoMode {
    return {
        codec: "h264",
        chroma: "yuv420",
        bitDepth: 8,
        primaries: "bt709",
        transfer: "bt709",
        matrix: "bt709",
        range: "limited",
        hdr: null,
        width: 1920,
        height: 1080,
        refreshMhz: 60000,
        vrr: false,
    }
}

/** Whether the transfer function is an HDR one. */
export function transferIsHdr(transfer: TransferFunction): boolean {
    return transfer === "pq" || transfer === "hlg"
}

/** The best codec present in a shared capability bitset (AV1 > HEVC > H264). */
export function bestCommonCodec(shared: number): VideoCodec {
    if ((shared & CODEC_BIT.av1) !== 0) return "av1"
    if ((shared & CODEC_BIT.hevc) !== 0) return "hevc"
    return "h264"
}

function supportsCodec(caps: Capabilities, codec: VideoCodec): boolean {
    // H.264 is the unconditional floor (see bestCommonCodec / CODEC_BIT doc), so
    // it is always supported even if the bit is unset; keeps a plain H.264
    // request from being reported as a downgrade. Mirrors media_caps.rs.
    return codec === "h264" || (caps.codecs & CODEC_BIT[codec]) !== 0
}

/** Reconcile a requested mode against both peers' capabilities. */
export function negotiate(
    client: Capabilities,
    host: Capabilities,
    requested: VideoMode,
): { mode: VideoMode; reason: DowngradeReasonName | null } {
    const out: VideoMode = { ...requested }
    let reason: DowngradeReasonName | null = null
    const setReason = (r: DowngradeReasonName) => {
        if (reason === null) reason = r
    }

    if (!(supportsCodec(client, requested.codec) && supportsCodec(host, requested.codec))) {
        out.codec = bestCommonCodec(client.codecs & host.codecs)
        setReason("capability_mismatch")
    }
    if (requested.chroma === "yuv444" && !(client.chroma444 && host.chroma444)) {
        out.chroma = "yuv420"
        setReason("capability_mismatch")
    }
    if (requested.bitDepth === 10 && !(client.bit10 && host.bit10)) {
        out.bitDepth = 8
        setReason("capability_mismatch")
    }
    const wantsHdr = transferIsHdr(requested.transfer) || requested.hdr !== null
    if (wantsHdr && !(client.hdr && host.hdr)) {
        out.hdr = null
        out.primaries = "bt709"
        out.transfer = "bt709"
        out.matrix = "bt709"
        out.range = "limited"
        setReason("unsupported")
    }
    const refreshCap = Math.min(client.maxRefreshMhz, host.maxRefreshMhz)
    if (requested.refreshMhz > refreshCap) {
        out.refreshMhz = refreshCap
        setReason("capability_mismatch")
    }
    if (requested.vrr && !(client.vrr && host.vrr)) {
        out.vrr = false
        setReason("capability_mismatch")
    }
    return { mode: out, reason }
}

// HDR mastering metadata wire codec (mirror of HdrMetadata::encode/decode).
//   u16 maxCLL | u16 maxFALL | u32 maxLum | u32 minLum (little-endian)
export const HDR_METADATA_WIRE_LEN = 12

/** Encode HDR mastering metadata for transport alongside the stream. */
export function encodeHdrMetadata(hdr: HdrMetadata): Uint8Array {
    const out = new Uint8Array(HDR_METADATA_WIRE_LEN)
    const view = new DataView(out.buffer)
    view.setUint16(0, hdr.maxCll & 0xffff, true)
    view.setUint16(2, hdr.maxFall & 0xffff, true)
    view.setUint32(4, hdr.maxLuminance >>> 0, true)
    view.setUint32(8, hdr.minLuminance >>> 0, true)
    return out
}

/** Decode HDR mastering metadata; null if shorter than HDR_METADATA_WIRE_LEN. */
export function decodeHdrMetadata(buf: ArrayBuffer | Uint8Array): HdrMetadata | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < HDR_METADATA_WIRE_LEN) return null
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    return {
        maxCll: view.getUint16(0, true),
        maxFall: view.getUint16(2, true),
        maxLuminance: view.getUint32(4, true),
        minLuminance: view.getUint32(8, true),
    }
}
