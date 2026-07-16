// FEC decode + reassembly pipe (U2 P1).
// Drop-in head for the "data" pipeline family:
//   baseType = "videodata", type = "wsdata"   (same as DepacketizeVideoPipe)
// Pipeline position: FecDecodePipe → VideoDecoderPipe / OpenH264 / … → renderer

import { Logger } from "../log.js"
import { Pipe, PipeInfo } from "../pipeline/index.js"
import { addPipePassthrough, DataPipe } from "../pipeline/pipes.js"
import { allVideoCodecs } from "../video.js"
import { DataVideoRenderer, VideoDecodeUnit, VideoRendererSetup } from "./index.js"
import { FecDecoder, FecDecoderStats } from "./fec.js"
import { parseSymbolMessage, parseChunkHeader, CHUNK_HEADER_SIZE } from "./fec_wire.js"

// ── Types ─────────────────────────────────────────────────────────────────

interface PendingFrame {
    /** 13-byte chunk header fields from the first chunk seen. */
    frameType: 0 | 1
    timestampUs: number
    chunkCount: number
    /** Sparse array; index = chunkIndex, value = fragment bytes. */
    parts: (Uint8Array | undefined)[]
    receivedCount: number
    /** Set once any delivered chunk arrived via FEC recovery rather than
     * direct receipt. Feeds stats.framesRecovered on completion. */
    usedRecovery: boolean
}

export interface FecDecodePipeOptions {
    /**
     * Called when highest_fully_decoded advances and the cadence threshold is
     * met: 32 delivered source symbols since last ack OR 50 ms elapsed.
     */
    onAck?: (highest: number) => void
    /** Injectable clock for tests. Default: () => Date.now() */
    now?: () => number
}

// Recovery/loss-span counters for one FecDecodePipe (U2 P2 groundwork).
// Symbol-level counters passthrough FecDecoder.getStats(); framesRecovered
// is tracked here, since only the chunk-reassembly seam knows which source
// seqs (recovered vs directly received) contributed to a completed frame.
export interface FecDecodePipeStats extends FecDecoderStats {
    framesRecovered: number
    framesDroppedAwaitingIdr: number
}

// ── Constants ─────────────────────────────────────────────────────────────

/** Maximum pending frame_ids before evicting the oldest (sets needsIdr). */
const MAX_PENDING_FRAMES = 8

/** ACK after this many delivered source symbols since last ack. */
const ACK_SYMBOL_INTERVAL = 32

/** ACK after this many ms since last ack (whichever comes first). */
const ACK_TIME_INTERVAL_MS = 50

// ── FecDecodePipe ─────────────────────────────────────────────────────────

export class FecDecodePipe implements DataPipe {

    static readonly baseType = "videodata"
    static readonly type = "wsdata"

    static async getInfo(): Promise<PipeInfo> {
        return {
            environmentSupported: true,
            supportedVideoCodecs: allVideoCodecs(),
        }
    }

    readonly implementationName: string

    private base: DataVideoRenderer
    private decoder: FecDecoder

    private framesRecovered = 0
    private framesDroppedAwaitingIdr = 0
    // Reassembly state
    private pending = new Map<number, PendingFrame>()
    // Eviction order — insertion-ordered frame_ids
    private pendingOrder: number[] = []

    // needsIdr flag
    private needsIdr = false
    // Unrecoverable loss invalidates predictive references. Delta frames stay
    // gated until a keyframe arrives, avoiding persistent decoder mushing.
    private awaitingIdr = false

    // Duration tracking (mirrors DepacketizeVideoPipe)
    private lastTimestampUs = 0

    // ACK state
    private onAck: ((highest: number) => void) | undefined
    private now: () => number
    private symbolsSinceAck = 0
    private lastAckTime = 0
    private lastAckedHighest: number | null = null
    // Independent 50ms timer: fires tickAck() even when no symbols arrive
    // (e.g. idle stream, reorder stall). Without this the encoder window stalls.
    private ackTimerId: ReturnType<typeof setInterval> | null = null

    constructor(base: DataVideoRenderer, logger?: Logger, options?: FecDecodePipeOptions) {
        this.implementationName = `fec_decode -> ${base.implementationName}`
        this.base = base
        // max_symbols=128 (design §4), max_bytes = 16 MiB
        this.decoder = new FecDecoder(128, 16 * 1024 * 1024)

        this.onAck = options?.onAck
        this.now = options?.now ?? (() => Date.now())
        this.lastAckTime = this.now()

        // Install a real-clock timer only when NOT using an injected test clock.
        // Test paths exercise the elapsed-time branch via symbol delivery with
        // an advanced fake clock; they do not need a live interval.
        if (!options?.now) {
            const t = setInterval(() => { this.tickAck() }, ACK_TIME_INTERVAL_MS)
            // In Node.js environments (tests), calling unref() prevents the timer
            // from keeping the process alive after all tests have finished.
            // In browsers, the returned value is a number and has no unref().
            if (typeof t === 'object' && t !== null && typeof (t as any).unref === 'function') {
                (t as any).unref()
            }
            this.ackTimerId = t
        }

        addPipePassthrough(this)
    }

    /** Cancel the background ACK timer. Call when the pipeline is torn down. */
    dispose(): void {
        if (this.ackTimerId !== null) {
            clearInterval(this.ackTimerId)
            this.ackTimerId = null
        }
    }

    /**
     * Public entry point for tests: simulate a timer tick without requiring
     * a real setInterval to fire.  Also called by the internal timer.
     */
    tickTimer(): void {
        this.tickAck()
    }

    /** Wire the ACK callback after construction (used by the pipeline builder). */
    setOnAck(fn: (highest: number) => void): void {
        this.onAck = fn
    }

    // ── DataPipe ──────────────────────────────────────────────────────────

    submitPacket(buffer: ArrayBuffer): void {
        const sym = parseSymbolMessage(buffer)
        if (!sym) return

        // Push through FEC decoder for both source and repair symbols.
        // For source symbols the decoder returns a Recovered event for the
        // symbol itself plus any additional symbols it can now recover; for
        // repair symbols it may recover previously missing source symbols.
        const events = this.decoder.pushSymbol(sym)
        for (const ev of events) {
            if (ev.kind === 'recovered') {
                this.deliverChunk(ev.payload, ev.viaFec)
                this.tickAck()
            } else if (ev.kind === 'lossSpan') {
                this.handleLossSpan(ev.fromSeq, ev.toSeqExclusive)
            }
        }
    }

    pollRequestIdr(): boolean {
        const v = this.needsIdr
        this.needsIdr = false
        // OR with base passthrough (e.g. decoder's own IDR request)
        const base = this.base as any
        if (typeof base.pollRequestIdr === 'function') {
            return v || base.pollRequestIdr()
        }
        return v
    }

    setup(setup: VideoRendererSetup) {
        if ("setup" in this.base && typeof (this.base as any).setup === "function") {
            return (this.base as any).setup(setup)
        }
    }

    getBase(): Pipe | null {
        return this.base
    }

    /** Snapshot of recovery/loss-span counters accumulated so far. Returns a
     * fresh copy; safe to call at any time. */
    getStats(): FecDecodePipeStats {
        return {
            ...this.decoder.getStats(),
            framesRecovered: this.framesRecovered,
            framesDroppedAwaitingIdr: this.framesDroppedAwaitingIdr,
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────

    /** Deliver one chunk (source-symbol payload) into the reassembly map. */
    private deliverChunk(payload: Uint8Array, viaFec: boolean): void {
        // payload = chunk layer: 13-byte header + Annex-B fragment
        if (payload.byteLength < CHUNK_HEADER_SIZE) return

        const hdr = parseChunkHeader(payload.buffer.slice(
            payload.byteOffset,
            payload.byteOffset + payload.byteLength,
        ) as ArrayBuffer)
        if (!hdr) return

        const { frameId, chunkIndex, chunkCount, frameType, timestampUs } = hdr
        const fragment = payload.slice(CHUNK_HEADER_SIZE)

        let frame = this.pending.get(frameId)
        if (!frame) {
            // Evict oldest if at cap
            if (this.pendingOrder.length >= MAX_PENDING_FRAMES) {
                const evictId = this.pendingOrder.shift()!
                this.pending.delete(evictId)
                this.needsIdr = true
                this.awaitingIdr = true
            }
            frame = {
                frameType,
                timestampUs,
                chunkCount,
                parts: new Array(chunkCount),
                receivedCount: 0,
                usedRecovery: false,
            }
            this.pending.set(frameId, frame)
            this.pendingOrder.push(frameId)
        }

        // Guard against duplicate delivery
        if (frame.parts[chunkIndex] !== undefined) return
        frame.usedRecovery = frame.usedRecovery || viaFec
        frame.parts[chunkIndex] = fragment
        frame.receivedCount++

        if (frame.receivedCount >= frame.chunkCount) {
            this.assembleFrame(frameId, frame)
        }
    }

    /** Concatenate a complete frame and submit it when its references are safe. */
    private assembleFrame(frameId: number, frame: PendingFrame): void {
        // Remove first so a gated delta cannot leak pending state.
        this.pending.delete(frameId)
        const pendingIdx = this.pendingOrder.indexOf(frameId)
        if (pendingIdx >= 0) this.pendingOrder.splice(pendingIdx, 1)

        if (this.awaitingIdr && frame.frameType !== 1) {
            this.framesDroppedAwaitingIdr++
            return
        }
        if (frame.frameType === 1) {
            this.awaitingIdr = false
        }
        // Concatenate fragments
        let totalLen = 0
        for (let i = 0; i < frame.chunkCount; i++) {
            totalLen += (frame.parts[i]?.byteLength ?? 0)
        }
        const data = new Uint8Array(totalLen)
        let offset = 0
        for (let i = 0; i < frame.chunkCount; i++) {
            const part = frame.parts[i]
            if (part) {
                data.set(part, offset)
                offset += part.byteLength
            }
        }
        if (frame.usedRecovery) {
            this.framesRecovered++
        }

        const duration = frame.timestampUs - this.lastTimestampUs
        this.lastTimestampUs = frame.timestampUs

        const unit: VideoDecodeUnit = {
            type: frame.frameType === 1 ? "key" : "delta",
            timestampMicroseconds: frame.timestampUs,
            durationMicroseconds: duration,
            data: data.buffer,
        }
        this.base.submitDecodeUnit(unit)

    }

    /**
     * A LossSpan means source seqs are unrecoverably gone. This invalidates
     * predictive references even if the lost frame contributed no chunks and
     * `pending` is empty. Request an IDR and gate deltas until it arrives.
     */
    private handleLossSpan(_from: number, _to: number): void {
        this.pending.clear()
        this.pendingOrder.length = 0
        this.needsIdr = true
        this.awaitingIdr = true
    }

    /**
     * ACK cadence gate — called both on symbol delivery and from the independent
     * 50ms timer.  Fires onAck when ≥32 symbols have been delivered since the
     * last ACK OR ≥50ms have elapsed, whichever comes first (design §2).
     *
     * The `seq` parameter is kept for compatibility with the old call-site but
     * is not used; the ACK value is highestFullyDecoded from the decoder.
     */
    private tickAck(): void {
        if (!this.onAck) return

        const hfd = this.decoder.highestFullyDecoded()
        if (hfd === null) return
        if (this.lastAckedHighest !== null && hfd <= this.lastAckedHighest) return

        this.symbolsSinceAck++
        const now = this.now()
        const elapsed = now - this.lastAckTime

        if (this.symbolsSinceAck >= ACK_SYMBOL_INTERVAL || elapsed >= ACK_TIME_INTERVAL_MS) {
            this.lastAckedHighest = hfd
            this.symbolsSinceAck = 0
            this.lastAckTime = now
            this.onAck(hfd)
        }
    }
}
