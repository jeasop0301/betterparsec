// Pure/testable seams for FEC session negotiation and DataChannel packet
// intake, extracted out of Stream (web/stream/index.ts) so the fatal/legacy
// negotiation branch and the accepted-packet heartbeat gating can be
// exercised directly by Node tests without duplicating the logic in
// production. See docs finding: G001 QA red team (agent://81-G001QaRedTeam).

import { FecPipeConfig } from "./fec_decode_pipe.js"

// ── Capability tuple resolution ────────────────────────────────────────────

/** The exact fields of StreamCapabilities this resolver reads. Kept minimal
 * and structural so this module never imports the (large) StreamCapabilities
 * type from the generated bindings. */
export interface FecCapabilityTuple {
    selected_fec_protocol_version?: number
    fec_epoch?: number
}

export type FecCapabilityResolution =
    /** Valid tuple, local FEC pipe present: wire the config into the pipe. */
    | { kind: "configure", config: FecPipeConfig }
    /** Valid legacy/v1 tuple, no local FEC pipe: the existing videotrack
     * fallback (already built by createVideoRenderer) is correct as-is. */
    | { kind: "fallback", config: FecPipeConfig }
    /** Selected v2 with no local pipe, or a self-contradictory tuple: fail
     * loudly rather than silently rendering nothing (or the wrong codec). */
    | { kind: "fatal", reason: string }

/**
 * Resolve the negotiated FEC capability tuple (from ConnectionComplete)
 * plus local pipe availability into exactly one outcome. Pure function —
 * no I/O, no logging, no mutation — so every branch is independently
 * testable. Mirrors the wire contract that used to live inline in
 * Stream.configureFecFromCapabilities.
 */
export function resolveFecCapability(capabilities: FecCapabilityTuple, hasPipe: boolean): FecCapabilityResolution {
    let config: FecPipeConfig
    if ((capabilities.selected_fec_protocol_version === undefined || capabilities.selected_fec_protocol_version === 1) &&
        capabilities.fec_epoch === undefined) {
        config = { version: 1 }
    } else if (capabilities.selected_fec_protocol_version === 2 &&
        Number.isInteger(capabilities.fec_epoch) && capabilities.fec_epoch! > 0 && capabilities.fec_epoch! <= 0xFFFF_FFFF) {
        config = { version: 2, epoch: capabilities.fec_epoch! }
    } else {
        return { kind: "fatal", reason: "Invalid negotiated FEC capability tuple" }
    }

    if (!hasPipe) {
        if (config.version === 2) {
            // Selected v2 media cannot be decoded without the FEC pipe/channels
            // (e.g. video_fec/video_fec_ack DataChannels never arrived before
            // ConnectionComplete) — fail loudly rather than silently dropping
            // to a videotrack pipeline that cannot render v2-only media.
            return { kind: "fatal", reason: "Host selected FEC v2 but no FEC channels/pipe are available on the client" }
        }
        // Legacy/v1 (or capability omitted) with no local FEC pipe:
        // createVideoRenderer() already fell back to the plain videotrack
        // pipeline because video_fec channels were absent — that fallback
        // is intentional and does not require FEC wiring.
        return { kind: "fallback", config }
    }

    return { kind: "configure", config }
}

// ── DataChannel packet intake ───────────────────────────────────────────────

/**
 * Normalize a video_fec DataChannel message to an ArrayBuffer, submit it
 * exactly once via `submit`, and invoke `onAccepted` iff `submit` returns
 * true. Blob-typed payloads (some environments' `binaryType` config) and
 * any unsupported data shape must never be treated as a live heartbeat —
 * they either normalize losslessly to an ArrayBuffer first, or the call
 * resolves to false without invoking `submit` or `onAccepted` at all.
 */
export async function handleFecDataMessage(
    data: unknown,
    submit: (buffer: ArrayBuffer) => boolean,
    onAccepted?: () => void,
): Promise<boolean> {
    let buffer: ArrayBuffer
    if (data instanceof ArrayBuffer) {
        buffer = data
    } else if (typeof Blob !== "undefined" && data instanceof Blob) {
        buffer = await data.arrayBuffer()
    } else {
        return false
    }

    const accepted = submit(buffer)
    if (accepted) {
        onAccepted?.()
    }
    return accepted
}
