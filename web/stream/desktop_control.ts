// Desktop control & ownership contract — TS mirror of
// common/src/desktop_control.rs. Keep the byte-pinned golden vector in
// tests/desktop_control.test.mjs in lockstep with the Rust
// `envelope_byte_pin_and_round_trip` test (change both or neither).
//
// Wire framing is little-endian (cursor / video_fec family):
//   u8 version | u8 domain | u16 kind | u32 generation | u32 payload_len | payload
//
// An unknown envelope version parses to null (forward-compat ignore); an
// unknown domain/kind still parses so the receiver can skip it. `generation`
// uses RFC 1982 (32-bit) wrap semantics and a stale generation can never
// mutate owned state (OwnershipLease).

export const CONTROL_VERSION = 1
export const CONTROL_HEADER_LEN = 12
export const CONTROL_MAX_PAYLOAD = 262144

export const CONTROL_DOMAIN_CURSOR = 1
export const CONTROL_DOMAIN_DISPLAY = 2
export const CONTROL_DOMAIN_PRIVACY = 3
export const CONTROL_DOMAIN_CLIPBOARD = 4
export const CONTROL_DOMAIN_FILE_TRANSFER = 5

// Stable downgrade-reason wire codes (0 = reserved / no reason). An unknown
// non-zero code is preserved as `{ name: 'unknown', code }` so telemetry never
// silently drops a reason it does not recognise.
export const DOWNGRADE_REASONS: Readonly<Record<number, string>> = {
    1: "unsupported",
    2: "capability_mismatch",
    3: "resource_busy",
    4: "stale_generation",
    5: "policy_denied",
    6: "transient_error",
    7: "version_mismatch",
}

export type DowngradeReason = { name: string; code: number }

/** Decodes a downgrade-reason code; null for the reserved 0. */
export function downgradeReason(code: number): DowngradeReason | null {
    if (code === 0) return null
    const name = DOWNGRADE_REASONS[code] ?? "unknown"
    return { name, code }
}

/** True when generation `a` is strictly newer than `b` (RFC 1982, 32-bit). */
export function generationSupersedes(a: number, b: number): boolean {
    const au = a >>> 0
    const bu = b >>> 0
    return au !== bu && ((au - bu) >>> 0) < 0x80000000
}

/**
 * Single-owner lease: admit() returns true when the caller may mutate owned
 * state (first claim / same-generation refresh / strictly newer takeover) and
 * false for a stale generation, which leaves the lease unchanged.
 */
export class OwnershipLease {
    private generation: number | null = null

    current(): number | null {
        return this.generation
    }

    admit(gen: number): boolean {
        const g = gen >>> 0
        if (this.generation === null) {
            this.generation = g
            return true
        }
        if (g === this.generation || generationSupersedes(g, this.generation)) {
            this.generation = g
            return true
        }
        return false
    }

    release(): void {
        this.generation = null
    }
}

export type ControlEnvelope = {
    domain: number
    kind: number
    generation: number
    payload: Uint8Array
}

/** Encodes a control envelope; null when payload exceeds CONTROL_MAX_PAYLOAD. */
export function encodeEnvelope(
    domain: number,
    kind: number,
    generation: number,
    payload: Uint8Array,
): Uint8Array | null {
    if (payload.length > CONTROL_MAX_PAYLOAD) return null
    const out = new Uint8Array(CONTROL_HEADER_LEN + payload.length)
    const view = new DataView(out.buffer)
    view.setUint8(0, CONTROL_VERSION)
    view.setUint8(1, domain & 0xff)
    view.setUint16(2, kind & 0xffff, true)
    view.setUint32(4, generation >>> 0, true)
    view.setUint32(8, payload.length, true)
    out.set(payload, CONTROL_HEADER_LEN)
    return out
}

/**
 * Decodes one control envelope. null for: a buffer shorter than the header, an
 * unknown version, a declared payload_len over CONTROL_MAX_PAYLOAD, or a body
 * shorter than declared. Trailing bytes beyond payload_len are tolerated.
 */
export function decodeEnvelope(buf: ArrayBuffer | Uint8Array): ControlEnvelope | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < CONTROL_HEADER_LEN) return null
    if (bytes[0] !== CONTROL_VERSION) return null
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const domain = view.getUint8(1)
    const kind = view.getUint16(2, true)
    const generation = view.getUint32(4, true)
    const payloadLen = view.getUint32(8, true)
    if (payloadLen > CONTROL_MAX_PAYLOAD) return null
    const end = CONTROL_HEADER_LEN + payloadLen
    if (bytes.length < end) return null
    return {
        domain,
        kind,
        generation,
        payload: bytes.slice(CONTROL_HEADER_LEN, end),
    }
}

// ── Display / output-selection domain (mirror of desktop_control::display) ──

export const DISPLAY_KIND_MODE_REQUEST = 1
export const DISPLAY_KIND_MODE_RESULT = 2
export const MODE_REQUEST_LEN = 10
export const MODE_RESULT_LEN = 12

export type ModeRequest = { outputId: number; width: number; height: number; refreshMhz: number }
export type TransitionStatus = "applied" | "downgraded" | "rejected"
export type ModeStatus = TransitionStatus
export type ModeResult = ModeRequest & { status: TransitionStatus; reason: DowngradeReason | null }

const TRANSITION_STATUS_CODES: Readonly<Record<TransitionStatus, number>> = { applied: 0, downgraded: 1, rejected: 2 }
const TRANSITION_STATUS_NAMES: Readonly<Record<number, TransitionStatus>> = { 0: "applied", 1: "downgraded", 2: "rejected" }

/** Encodes a display mode request (little-endian, fixed MODE_REQUEST_LEN). */
export function encodeModeRequest(req: ModeRequest): Uint8Array {
    const out = new Uint8Array(MODE_REQUEST_LEN)
    const view = new DataView(out.buffer)
    view.setUint16(0, req.outputId & 0xffff, true)
    view.setUint16(2, req.width & 0xffff, true)
    view.setUint16(4, req.height & 0xffff, true)
    view.setUint32(6, req.refreshMhz >>> 0, true)
    return out
}

/** Decodes a display mode request; null if shorter than MODE_REQUEST_LEN. */
export function decodeModeRequest(buf: ArrayBuffer | Uint8Array): ModeRequest | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < MODE_REQUEST_LEN) return null
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    return {
        outputId: view.getUint16(0, true),
        width: view.getUint16(2, true),
        height: view.getUint16(4, true),
        refreshMhz: view.getUint32(6, true),
    }
}

/** Encodes a display mode result (little-endian, fixed MODE_RESULT_LEN). */
export function encodeModeResult(res: ModeResult): Uint8Array {
    const out = new Uint8Array(MODE_RESULT_LEN)
    const view = new DataView(out.buffer)
    view.setUint16(0, res.outputId & 0xffff, true)
    view.setUint16(2, res.width & 0xffff, true)
    view.setUint16(4, res.height & 0xffff, true)
    view.setUint32(6, res.refreshMhz >>> 0, true)
    view.setUint8(10, TRANSITION_STATUS_CODES[res.status])
    view.setUint8(11, res.reason ? res.reason.code & 0xff : 0)
    return out
}

/** Decodes a display mode result; null if truncated or the status is unknown. */
export function decodeModeResult(buf: ArrayBuffer | Uint8Array): ModeResult | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < MODE_RESULT_LEN) return null
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const status = TRANSITION_STATUS_NAMES[view.getUint8(10)]
    if (status === undefined) return null
    return {
        outputId: view.getUint16(0, true),
        width: view.getUint16(2, true),
        height: view.getUint16(4, true),
        refreshMhz: view.getUint32(6, true),
        status,
        reason: downgradeReason(view.getUint8(11)),
    }
}

// ── Privacy / display-lease domain (mirror of desktop_control::privacy) ─────

export const PRIVACY_KIND_REQUEST = 1
export const PRIVACY_KIND_STATE = 2
export const PROTECT_BLANK_DISPLAY = 0x01
export const PROTECT_BLOCK_LOCAL_INPUT = 0x02
export const PROTECT_ALL = PROTECT_BLANK_DISPLAY | PROTECT_BLOCK_LOCAL_INPUT
export const PRIVACY_REQUEST_LEN = 2
export const PRIVACY_STATE_LEN = 4

export type PrivacyRequest = { enable: boolean; protections: number }
export type PrivacyState = {
    active: boolean
    protectionsEffective: number
    status: TransitionStatus
    reason: DowngradeReason | null
}

/** Encodes a privacy request (fixed PRIVACY_REQUEST_LEN). */
export function encodePrivacyRequest(req: PrivacyRequest): Uint8Array {
    return new Uint8Array([req.enable ? 1 : 0, req.protections & 0xff])
}

/** Decodes a privacy request; null if truncated or a reserved bit is set. */
export function decodePrivacyRequest(buf: ArrayBuffer | Uint8Array): PrivacyRequest | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < PRIVACY_REQUEST_LEN) return null
    const protections = bytes[1]
    if ((protections & ~PROTECT_ALL) !== 0) return null
    return { enable: bytes[0] !== 0, protections }
}

/** Encodes a privacy state (fixed PRIVACY_STATE_LEN). */
export function encodePrivacyState(state: PrivacyState): Uint8Array {
    return new Uint8Array([
        state.active ? 1 : 0,
        state.protectionsEffective & 0xff,
        TRANSITION_STATUS_CODES[state.status],
        state.reason ? state.reason.code & 0xff : 0,
    ])
}

/** Decodes a privacy state; null if truncated or the status is unknown. */
export function decodePrivacyState(buf: ArrayBuffer | Uint8Array): PrivacyState | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < PRIVACY_STATE_LEN) return null
    const status = TRANSITION_STATUS_NAMES[bytes[2]]
    if (status === undefined) return null
    return {
        active: bytes[0] !== 0,
        protectionsEffective: bytes[1],
        status,
        reason: downgradeReason(bytes[3]),
    }
}

// ── Cursor-authority domain (mirror of desktop_control::cursor) ────────────

export const CURSOR_KIND_AUTHORITY = 1
export const CURSOR_AUTHORITY_LEN = 1
export type CursorOwner = "host" | "client"

const CURSOR_OWNER_CODES: Readonly<Record<CursorOwner, number>> = { host: 0, client: 1 }
const CURSOR_OWNER_NAMES: Readonly<Record<number, CursorOwner>> = { 0: "host", 1: "client" }

/** Encodes a cursor-authority message. */
export function encodeCursorAuthority(owner: CursorOwner): Uint8Array {
    return new Uint8Array([CURSOR_OWNER_CODES[owner]])
}

/** Decodes a cursor-authority message; null if truncated or the code is unknown. */
export function decodeCursorAuthority(buf: ArrayBuffer | Uint8Array): CursorOwner | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < CURSOR_AUTHORITY_LEN) return null
    const owner = CURSOR_OWNER_NAMES[bytes[0]]
    return owner === undefined ? null : owner
}

// ── Clipboard-kind domain (mirror of desktop_control::clipboard) ───────────

export const CLIPBOARD_KIND_OFFER = 1
export const CLIPBOARD_KIND_REQUEST = 2
export const CLIP_TEXT = 0x01
export const CLIP_PNG = 0x02
export const CLIP_FILE_LIST = 0x04
export const CLIP_ALL = CLIP_TEXT | CLIP_PNG | CLIP_FILE_LIST

/** Encodes a clipboard offer bitset (unknown bits masked off). */
export function encodeClipboardOffer(kinds: number): Uint8Array {
    return new Uint8Array([kinds & CLIP_ALL])
}

/** Decodes a clipboard offer; unknown bits masked to known kinds. null if truncated. */
export function decodeClipboardOffer(buf: ArrayBuffer | Uint8Array): number | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < 1) return null
    return bytes[0] & CLIP_ALL
}

/** Encodes a single-kind clipboard request. */
export function encodeClipboardRequest(kind: number): Uint8Array {
    return new Uint8Array([kind & 0xff])
}

/** Decodes a clipboard request; null unless exactly one known kind bit is set. */
export function decodeClipboardRequest(buf: ArrayBuffer | Uint8Array): number | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < 1) return null
    const kind = bytes[0]
    if ((kind & ~CLIP_ALL) !== 0 || kind === 0 || (kind & (kind - 1)) !== 0) return null
    return kind
}

// ── File-transfer control (mirror of desktop_control::file_transfer) ────────

export const FT_KIND_OFFER = 1
export const FT_KIND_ACCEPT = 2
export const FT_KIND_CANCEL = 3
export const FT_KIND_PROGRESS = 4
export const FT_OFFER_HEADER_LEN = 14
export const FT_MAX_NAME_LEN = 1024

export type FileOffer = { transferId: number; totalSize: number; name: string }

// u64 little-endian split into two u32 halves using Number. Safe for values
// below 2^53 (~9 PB) — ample for file sizes/progress — and avoids BigInt, which
// this project's pre-es2020 TS target does not support.
const U32_MOD = 0x100000000
function writeU64LE(view: DataView, offset: number, value: number): void {
    view.setUint32(offset, value % U32_MOD, true)
    view.setUint32(offset + 4, Math.floor(value / U32_MOD), true)
}
function readU64LE(view: DataView, offset: number): number {
    const low = view.getUint32(offset, true)
    const high = view.getUint32(offset + 4, true)
    return high * U32_MOD + low
}

/** Encodes a file offer; null if the UTF-8 name exceeds FT_MAX_NAME_LEN bytes. */
export function encodeFileOffer(offer: FileOffer): Uint8Array | null {
    const name = new TextEncoder().encode(offer.name)
    if (name.length > FT_MAX_NAME_LEN) return null
    const out = new Uint8Array(FT_OFFER_HEADER_LEN + name.length)
    const view = new DataView(out.buffer)
    view.setUint32(0, offer.transferId >>> 0, true)
    writeU64LE(view, 4, offer.totalSize)
    view.setUint16(12, name.length, true)
    out.set(name, FT_OFFER_HEADER_LEN)
    return out
}

/** Decodes a file offer; null on truncation, over-cap name, or invalid UTF-8. */
export function decodeFileOffer(buf: ArrayBuffer | Uint8Array): FileOffer | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < FT_OFFER_HEADER_LEN) return null
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const transferId = view.getUint32(0, true)
    const totalSize = readU64LE(view, 4)
    const nameLen = view.getUint16(12, true)
    if (nameLen > FT_MAX_NAME_LEN) return null
    const end = FT_OFFER_HEADER_LEN + nameLen
    if (bytes.length < end) return null
    try {
        const name = new TextDecoder("utf-8", { fatal: true }).decode(bytes.slice(FT_OFFER_HEADER_LEN, end))
        return { transferId, totalSize, name }
    } catch {
        return null
    }
}

/** Encodes a file-transfer accept (u32 transfer_id). */
export function encodeFileAccept(transferId: number): Uint8Array {
    const out = new Uint8Array(4)
    new DataView(out.buffer).setUint32(0, transferId >>> 0, true)
    return out
}

/** Decodes a file-transfer accept; null if truncated. */
export function decodeFileAccept(buf: ArrayBuffer | Uint8Array): number | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < 4) return null
    return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0, true)
}

/** Encodes a file-transfer cancel (u32 transfer_id | u8 reason; 0 = none). */
export function encodeFileCancel(transferId: number, reason: DowngradeReason | null): Uint8Array {
    const out = new Uint8Array(5)
    new DataView(out.buffer).setUint32(0, transferId >>> 0, true)
    out[4] = reason ? reason.code & 0xff : 0
    return out
}

/** Decodes a file-transfer cancel; null if truncated. */
export function decodeFileCancel(buf: ArrayBuffer | Uint8Array): { transferId: number; reason: DowngradeReason | null } | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < 5) return null
    const transferId = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0, true)
    return { transferId, reason: downgradeReason(bytes[4]) }
}

/** Encodes file-transfer progress (u32 transfer_id | u64 bytes_done). */
export function encodeFileProgress(transferId: number, bytesDone: number): Uint8Array {
    const out = new Uint8Array(12)
    const view = new DataView(out.buffer)
    view.setUint32(0, transferId >>> 0, true)
    writeU64LE(view, 4, bytesDone)
    return out
}

/** Decodes file-transfer progress; null if truncated. */
export function decodeFileProgress(buf: ArrayBuffer | Uint8Array): { transferId: number; bytesDone: number } | null {
    const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf)
    if (bytes.length < 12) return null
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    return { transferId: view.getUint32(0, true), bytesDone: readU64LE(view, 4) }
}

// ── Client-side output selection (mirror of desktop_control::display::OutputSelection) ──

export type AppliedMode = { outputId: number; width: number; height: number; refreshMhz: number }

export class OutputSelection {
    private appliedMode: AppliedMode | null = null
    private pendingReq: ModeRequest | null = null

    /** Records an in-flight mode request (requested side of the telemetry). */
    request(req: ModeRequest): void {
        this.pendingReq = req
    }

    /** The in-flight request, if any. */
    pending(): ModeRequest | null {
        return this.pendingReq
    }

    /** The last confirmed applied mode, if any. */
    applied(): AppliedMode | null {
        return this.appliedMode
    }

    /** Reconciles a host ModeResult; applied/downgraded adopt the effective mode,
     * rejected keeps the previous (rollback). Clears the in-flight request. */
    reconcile(result: ModeResult): TransitionStatus {
        this.pendingReq = null
        if (result.status === "applied" || result.status === "downgraded") {
            this.appliedMode = {
                outputId: result.outputId,
                width: result.width,
                height: result.height,
                refreshMhz: result.refreshMhz,
            }
        }
        return result.status
    }

    /** Hot-unplug: if the active output was removed, clears it and stages/returns
     * a fallback request; otherwise null (unaffected). */
    onOutputRemoved(outputId: number, fallbackOutput: number): ModeRequest | null {
        if (this.appliedMode !== null && this.appliedMode.outputId === outputId) {
            this.appliedMode = null
            const req: ModeRequest = { outputId: fallbackOutput, width: 0, height: 0, refreshMhz: 0 }
            this.pendingReq = req
            return req
        }
        return null
    }
}
