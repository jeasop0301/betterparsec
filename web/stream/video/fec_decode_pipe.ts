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
import {
    CHUNK_COUNT_MAX,
    CHUNK_HEADER_SIZE,
    CHUNK_V2_FRAGMENT_MAX,
    CHUNK_V2_HEADER_SIZE,
    FRAME_MAX_BYTES,
    parseChunkHeader,
    parseChunkHeaderV2,
    parseSymbolMessage,
    verifyEncodedFrameV2,
} from "./fec_wire.js"

// ── Types ─────────────────────────────────────────────────────────────────

interface PendingFrame {
    epoch: number
    version: 1 | 2
    frameType: 0 | 1
    timestampUs: number
    chunkCount: number
    encodedFrameLen: number
    encodedFrameCrc32: number | null
    parts: (Uint8Array | undefined)[]
    receivedCount: number
    bytes: number
    usedRecovery: boolean
}

interface CompletedFrame {
    epoch: number
    frameId: number
    frame: PendingFrame
    completedAt: number
}

export type DiscontinuityReason =
    | "epoch-transition"
    | "reorder-gap"
    | "reassembly-eviction"
    | "metadata-mismatch"
    | "frame-crc"
    | "memory-cap"
    | "fec-eviction"

export interface Discontinuity {
    epoch: number
    reason: DiscontinuityReason
}

export type FecPipeConfig =
    | { version: 1 }
    | { version: 2, epoch: number }

export interface FecDecodePipeOptions {
    /**
     * Called when highest_fully_decoded advances and the cadence threshold is
     * met: 32 delivered source symbols since last ack OR 50 ms elapsed.
     */
    onAck?: (highest: number) => void
    /** Injectable clock for tests. Default: () => Date.now() */
    now?: () => number
    /** Observable transport discontinuity; decoder acknowledgement is separate. */
    onDiscontinuity?: (discontinuity: Discontinuity) => void
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

/** Bounded incomplete frames and completed reorder records. */
const MAX_PENDING_FRAMES = 8
const MAX_COMPLETED_FRAMES = 4
const REORDER_MAX_WAIT_MS = 100
const MAX_REASSEMBLY_REORDER_BYTES = 16 * 1024 * 1024

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
    private pending = new Map<string, PendingFrame>()
    private completed = new Map<string, CompletedFrame>()
    private completedOrder: string[] = []
    private bytesBuffered = 0
    private config: FecPipeConfig = { version: 1 }
    private hasExplicitConfig = false
    private nextFrameId: number | null = null

    // needsIdr flag
    private needsIdr = false
    // Unrecoverable loss invalidates predictive references. Delta frames stay
    // gated until a keyframe arrives, avoiding persistent decoder mushing.
    private awaitingIdr = false

    // Duration tracking (mirrors DepacketizeVideoPipe)
    private lastTimestampUs = 0

    // ACK state
    private onAck: ((highest: number) => void) | undefined
    private onDiscontinuity: ((discontinuity: Discontinuity) => void) | undefined
    private now: () => number
    private symbolsSinceAck = 0
    private lastAckTime = 0
    private lastAckedHighest: number | null = null
    private ackTimerId: ReturnType<typeof setInterval> | null = null

    constructor(base: DataVideoRenderer, logger?: Logger, options?: FecDecodePipeOptions) {
        this.implementationName = `fec_decode -> ${base.implementationName}`
        this.base = base
        // max_symbols=128 (design §4), max_bytes = 16 MiB
        this.decoder = new FecDecoder(128, 16 * 1024 * 1024)

        this.onAck = options?.onAck
        this.onDiscontinuity = options?.onDiscontinuity
        this.now = options?.now ?? (() => Date.now())
        this.lastAckTime = this.now()

        // Install a real-clock timer only when NOT using an injected test clock.
        // Test paths exercise the elapsed-time branch via symbol delivery with
        // an advanced fake clock; they do not need a live interval.
        if (!options?.now) {
            const t = setInterval(() => this.tickTimer(), ACK_TIME_INTERVAL_MS)
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

    private cleanedUp = false

    /**
     * Idempotent teardown for this pipe's own resources (ACK timer/decoder
     * state), then forwards to the wrapped pipe's cleanup exactly once.
     * addPipePassthrough() only installs a generic forwarding "cleanup" when
     * the pipe doesn't already own one — declaring this method here means
     * production renderer teardown (Stream.createVideoRenderer ->
     * videoRenderer.cleanup()) actually disposes the timer instead of
     * silently skipping straight to base.cleanup().
     */
    cleanup(): void {
        if (this.cleanedUp) return
        this.cleanedUp = true
        this.dispose()
        const base = this.base as any
        if (base && typeof base.cleanup === "function") {
            base.cleanup()
        }
    }

    /**
     * Public entry point for tests: simulate a timer tick without requiring
     * a real setInterval to fire.  Also called by the internal timer.
     */
    tickTimer(): void {
        this.flushReorder(this.now())
        this.tickAck(false)
    }

    /** Wire the ACK callback after construction (used by the pipeline builder). */
    setOnAck(fn: (highest: number) => void): void {
        this.onAck = fn
    }
    /** Latch the FEC codec negotiated in ConnectionComplete before admitting media. */
    configure(config: FecPipeConfig): boolean {
        if (config.version === 2 &&
            (!Number.isInteger(config.epoch) || config.epoch <= 0 || config.epoch > 0xFFFF_FFFF)) {
            return false
        }
        const firstExplicitConfig = !this.hasExplicitConfig
        const changed = !firstExplicitConfig && (this.config.version !== config.version ||
            (config.version === 2 && this.config.version === 2 && this.config.epoch !== config.epoch))
        this.config = config
        this.hasExplicitConfig = true
        if (firstExplicitConfig && config.version === 2) {
            this.decoder = new FecDecoder(128, 16 * 1024 * 1024)
            this.resetEpochState()
            this.pending.clear()
            this.completed.clear()
            this.completedOrder.length = 0
            this.bytesBuffered = 0
            this.nextFrameId = null
            this.needsIdr = false
            this.awaitingIdr = true
        } else if (changed) {
            this.decoder = new FecDecoder(128, 16 * 1024 * 1024)
            this.resetEpochState()
            this.discontinue(config.version === 2 ? config.epoch : 0, "epoch-transition")
        }
        return true
    }

    // ── DataPipe ──────────────────────────────────────────────────────────

    submitPacket(buffer: ArrayBuffer): boolean {
        const sym = parseSymbolMessage(buffer)
        if (!sym || !this.config) return false
        if (sym.version !== this.config.version) return false
        const epoch = sym.version === 2 ? sym.epoch! : 0
        if (this.config.version === 2 && epoch !== this.config.epoch) return false
        const events = this.decoder.pushSymbol(sym)
        for (const ev of events) {
            if (ev.kind === "recovered") {
                this.deliverChunk(epoch, sym.version, ev.payload, ev.viaFec)
                this.tickAck(true)
            } else if (ev.kind === "lossSpan" || ev.kind === "evicted") {
                this.discontinue(epoch, "fec-eviction")
            }
        }
        this.flushReorder(this.now())
        return true
    }

    pollRequestIdr(): boolean {
        const v = this.needsIdr
        this.needsIdr = false
        const base = this.base as any
        if (typeof base.pollRequestIdr === "function") return v || base.pollRequestIdr()
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

    getStats(): FecDecodePipeStats {
        return {
            ...this.decoder.getStats(),
            framesRecovered: this.framesRecovered,
            framesDroppedAwaitingIdr: this.framesDroppedAwaitingIdr,
        }
    }

    private key(epoch: number, frameId: number): string {
        return `${epoch}:${frameId >>> 0}`
    }

    private resetEpochState(): void {
        this.lastTimestampUs = 0
        this.symbolsSinceAck = 0
        this.lastAckedHighest = null
        this.lastAckTime = this.now()
    }

    private u32Newer(candidate: number, current: number): boolean {
        const distance = (candidate - current) >>> 0
        return distance !== 0 && distance < 0x80000000
    }

    private discontinue(epoch: number, reason: DiscontinuityReason): void {
        this.pending.clear()
        this.completed.clear()
        this.completedOrder.length = 0
        this.bytesBuffered = 0
        this.nextFrameId = null
        this.needsIdr = true
        this.awaitingIdr = true
        this.onDiscontinuity?.({ epoch, reason })
    }

    private deliverChunk(epoch: number, version: 1 | 2, payload: Uint8Array, viaFec: boolean): void {
        const wire = payload.buffer.slice(payload.byteOffset, payload.byteOffset + payload.byteLength) as ArrayBuffer
        const hdr = version === 2 ? parseChunkHeaderV2(wire) : parseChunkHeader(wire)
        if (!hdr || hdr.frameType > 1 || hdr.chunkCount === 0 || hdr.chunkCount > CHUNK_COUNT_MAX ||
            hdr.chunkIndex >= hdr.chunkCount) return
        const headerBytes = version === 2 ? CHUNK_V2_HEADER_SIZE : CHUNK_HEADER_SIZE
        const fragment = payload.slice(headerBytes)
        const v2Hdr = version === 2 ? hdr as typeof hdr & {
            encodedFrameLen: number
            encodedFrameCrc32: number
        } : null
        const encodedFrameLen = v2Hdr?.encodedFrameLen ?? 0
        const encodedFrameCrc32 = v2Hdr?.encodedFrameCrc32 ?? null
        if (v2Hdr && (
            encodedFrameLen > FRAME_MAX_BYTES ||
            hdr.chunkCount !== Math.ceil(Math.max(encodedFrameLen, 1) / CHUNK_V2_FRAGMENT_MAX) ||
            fragment.byteLength !== (hdr.chunkIndex + 1 === hdr.chunkCount
                ? encodedFrameLen - CHUNK_V2_FRAGMENT_MAX * (hdr.chunkCount - 1)
                : CHUNK_V2_FRAGMENT_MAX)
        )) return

        const key = this.key(epoch, hdr.frameId)
        if (this.completed.has(key) ||
            (this.nextFrameId !== null && hdr.frameId !== this.nextFrameId && !this.u32Newer(hdr.frameId, this.nextFrameId))) {
            return
        }
        let frame = this.pending.get(key)
        if (!frame) {
            if (this.pending.size + this.completed.size >= MAX_PENDING_FRAMES) {
                this.discontinue(epoch, "reassembly-eviction")
                return
            }
            if (this.bytesBuffered + fragment.byteLength > MAX_REASSEMBLY_REORDER_BYTES) {
                this.discontinue(epoch, "memory-cap")
                return
            }
            frame = {
                epoch,
                version,
                frameType: hdr.frameType,
                timestampUs: hdr.timestampUs,
                chunkCount: hdr.chunkCount,
                encodedFrameLen,
                encodedFrameCrc32,
                parts: new Array(hdr.chunkCount),
                receivedCount: 0,
                bytes: 0,
                usedRecovery: false,
            }
            this.pending.set(key, frame)
            // While awaitingIdr, an incomplete delta must never claim the
            // post-reset ordering anchor: only the first pending key frame
            // may become nextFrameId, so a stale/incomplete delta cannot
            // stall a later complete key behind a reorder-gap wait.
            if (this.nextFrameId === null && (!this.awaitingIdr || hdr.frameType === 1)) this.nextFrameId = hdr.frameId
        } else if (
            frame.version !== version ||
            frame.frameType !== hdr.frameType ||
            frame.timestampUs !== hdr.timestampUs ||
            frame.chunkCount !== hdr.chunkCount ||
            frame.encodedFrameLen !== encodedFrameLen ||
            frame.encodedFrameCrc32 !== encodedFrameCrc32
        ) {
            this.discontinue(epoch, "metadata-mismatch")
            return
        }

        if (frame.parts[hdr.chunkIndex] !== undefined) return
        if (this.bytesBuffered + fragment.byteLength > MAX_REASSEMBLY_REORDER_BYTES) {
            this.discontinue(epoch, "memory-cap")
            return
        }
        frame.usedRecovery ||= viaFec
        frame.parts[hdr.chunkIndex] = fragment
        frame.receivedCount++
        frame.bytes += fragment.byteLength
        this.bytesBuffered += fragment.byteLength

        if (frame.receivedCount === frame.chunkCount) {
            this.pending.delete(key)
            if (this.awaitingIdr && frame.frameType !== 1) {
                this.bytesBuffered -= frame.bytes
                this.framesDroppedAwaitingIdr++
                // A gated delta cannot anchor reordering: the next admitted
                // keyframe establishes the recovered decode reference.
                if (this.nextFrameId === hdr.frameId) this.nextFrameId = null
                return
            }
            if (frame.frameType === 1) this.awaitingIdr = false
            this.completed.set(key, { epoch, frameId: hdr.frameId, frame, completedAt: this.now() })
            this.completedOrder.push(key)
        }
    }

    private flushReorder(now: number): void {
        while (this.completedOrder.length > 0) {
            const firstKey = this.completedOrder[0]
            const first = this.completed.get(firstKey)!
            const expectedKey = this.nextFrameId === null ? firstKey : this.key(first.epoch, this.nextFrameId)
            let next = this.completed.get(expectedKey)
            if (!next) {
                if (this.completedOrder.length <= MAX_COMPLETED_FRAMES && now - first.completedAt < REORDER_MAX_WAIT_MS) return
                this.discontinue(first.epoch, "reorder-gap")
                return
            }
            this.completed.delete(expectedKey)
            this.completedOrder.splice(this.completedOrder.indexOf(expectedKey), 1)
            this.emitFrame(next)
            this.nextFrameId = (next.frameId + 1) >>> 0
        }
    }

    private emitFrame(completed: CompletedFrame): void {
        const { frame } = completed
        const data = new Uint8Array(frame.bytes)
        let offset = 0
        for (const part of frame.parts) {
            if (!part) return
            data.set(part, offset)
            offset += part.byteLength
        }
        this.bytesBuffered -= frame.bytes
        if (frame.version === 2 && !verifyEncodedFrameV2({
            frameId: completed.frameId,
            chunkIndex: 0,
            chunkCount: frame.chunkCount,
            frameType: frame.frameType,
            timestampUs: frame.timestampUs,
            encodedFrameLen: frame.encodedFrameLen,
            encodedFrameCrc32: frame.encodedFrameCrc32!,
        }, data)) {
            this.discontinue(completed.epoch, "frame-crc")
            return
        }
        if (frame.usedRecovery) this.framesRecovered++
        const duration = frame.timestampUs - this.lastTimestampUs
        this.lastTimestampUs = frame.timestampUs
        this.base.submitDecodeUnit({
            type: frame.frameType === 1 ? "key" : "delta",
            timestampMicroseconds: frame.timestampUs,
            durationMicroseconds: duration,
            data: data.buffer,
        })
    }

    /**
     * ACK cadence gate — called both on symbol delivery and from the independent
     * 50ms timer.  Fires onAck when ≥32 symbols have been delivered since the
     * last ACK OR ≥50ms have elapsed, whichever comes first (design §2).
     *
     * `delivered` distinguishes the two call sites: only a delivered source
     * symbol (submitPacket -> "recovered" event) advances symbolsSinceAck;
     * the independent 50ms timer (tickTimer) must never bump the symbol
     * counter, or the "32 delivered symbols" cadence drifts on idle links.
     */
    private tickAck(delivered: boolean): void {
        if (!this.onAck) return

        const hfd = this.decoder.highestFullyDecoded()
        if (hfd === null) return
        if (this.lastAckedHighest !== null && !this.u32Newer(hfd, this.lastAckedHighest)) return

        if (delivered) this.symbolsSinceAck++
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
