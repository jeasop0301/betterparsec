import { StreamerStatsUpdate, TransportChannelId } from "../api_bindings.js"
import { BIG_BUFFER, ByteBuffer } from "./buffer.js"
import { Logger } from "./log.js"
import { Pipe } from "./pipeline/index.js"
import { DataTransportChannel, Transport } from "./transport/index.js"

export type StatValue = string | number

export type StreamStatsData = {
    videoCodec: string | null
    videoWidth: number | null
    videoHeight: number | null
    videoFps: number | null
    videoPipeline: string | null
    audioPipeline: string | null
    hdrEnabled: boolean | null
    streamerRttMs: number | null
    streamerRttVarianceMs: number | null
    minHostProcessingLatencyMs: number | null
    maxHostProcessingLatencyMs: number | null
    avgHostProcessingLatencyMs: number | null
    minStreamerProcessingTimeMs: number | null
    maxStreamerProcessingTimeMs: number | null
    avgStreamerProcessingTimeMs: number | null
    browserRtt: number | null
    transport: Record<string, StatValue>
    streamerVideoTransport: Record<string, StatValue>
    runtimeBitrateControl: Record<string, StatValue>
    video: Record<string, StatValue>
    audio: Record<string, StatValue>
}

export type BenchmarkSample = {
    sequence: number
    elapsedMs: number
    collectedAt: string
    freshnessMs: {
        transport: number | null
        streamerRtt: number | null
        streamerVideo: number | null
        streamerVideoTransport: number | null
        streamerBrowserRtt: number | null
        runtimeBitrateControl: number | null
        video: number | null
        audio: number | null
    }
    stats: StreamStatsData
}

export type BenchmarkStreamSettings = {
    bitrateKbps: number
    width: number
    height: number
    fps: number
    requestedVideoCodec: string
    requestedHdr: boolean
    requestedDataTransport: string
    iceTransportPolicy: string
    playAudioLocal: boolean
    videoFrameQueueSize: number
    audioSampleQueueSize: number
}

export type BenchmarkContext = {
    pageUrl: string
    userAgent: string
    streamSettings: BenchmarkStreamSettings
}

export type BenchmarkReport = {
    schemaVersion: 2
    startedAt: string | null
    endedAt: string
    exportedAt: string
    sampleWindowStartedAt: string | null
    sampleWindowEndedAt: string | null
    application: {
        name: "betterparsec"
        version: "unknown"
        build: "unknown"
        gitSha: "unknown"
    }
    host: {
        softwareVersion: "unknown"
        gpuDriverVersion: "unknown"
        gitSha: "unknown"
    }
    browser: {
        userAgent: string
    }
    page: {
        url: string
        queryParametersIncluded: false
    }
    streamSettings: BenchmarkStreamSettings
    selected: {
        videoCodec: string | null
        videoPipeline: string | null
        audioPipeline: string | null
        hdrEnabled: boolean | null
    }
    sampleSequence: {
        first: number | null
        last: number | null
        count: number
        retainedLimit: number
    }
    samples: Array<BenchmarkSample>
    limitations: Array<string>
}

const UNKNOWN = "unknown" as const

export function sanitizeBenchmarkPageUrl(rawUrl: string): string {
    try {
        const url = new URL(rawUrl)
        return `${url.origin}${url.pathname}`
    } catch (_error) {
        return UNKNOWN
    }
}

function num(value: number | null | undefined, suffix?: string): string | null {
    if (value == null) {
        return null
    } else {
        return `${value.toFixed(2)}${suffix ?? ""}`
    }
}

export function streamStatsToText(statsData: StreamStatsData): string {
    let text = `stats:
video information: ${statsData.videoCodec}, ${statsData.videoWidth}x${statsData.videoHeight}, ${statsData.videoFps} fps
HDR: ${statsData.hdrEnabled === true ? "Enabled" : statsData.hdrEnabled === false ? "Disabled" : "Unknown"}
video pipeline: ${statsData.videoPipeline}
audio pipeline: ${statsData.audioPipeline}
streamer round trip time: ${num(statsData.streamerRttMs, "ms")} (variance: ${num(statsData.streamerRttVarianceMs, "ms")})
host processing latency min/max/avg: ${num(statsData.minHostProcessingLatencyMs, "ms")} / ${num(statsData.maxHostProcessingLatencyMs, "ms")} / ${num(statsData.avgHostProcessingLatencyMs, "ms")}
streamer processing latency min/max/avg: ${num(statsData.minStreamerProcessingTimeMs, "ms")} / ${num(statsData.maxStreamerProcessingTimeMs, "ms")} / ${num(statsData.avgStreamerProcessingTimeMs, "ms")}
streamer to browser rtt (ws only): ${num(statsData.browserRtt, "ms")}
`
    for (const key in statsData.transport) {
        const value = statsData.transport[key]
        let valuePretty = value

        if (typeof value == "number" && key.endsWith("Ms")) {
            valuePretty = `${num(value, "ms")}`
        }

        text += `${key}: ${valuePretty}\n`
    }

    for (const key in statsData.streamerVideoTransport) {
        const value = statsData.streamerVideoTransport[key]
        let valuePretty = value

        if (typeof value == "number" && key.endsWith("Ms")) {
            valuePretty = `${num(value, "ms")}`
        }

        text += `streamerVideoTransport.${key}: ${valuePretty}\n`
    }

    for (const key in statsData.runtimeBitrateControl) {
        text += `runtimeBitrateControl.${key}: ${statsData.runtimeBitrateControl[key]}\n`
    }

    for (const key in statsData.video) {
        const value = statsData.video[key]
        let valuePretty = value

        if (typeof value == "number" && key.endsWith("Ms")) {
            valuePretty = `${num(value, "ms")}`
        }

        text += `${key}: ${valuePretty}\n`
    }

    for (const key in statsData.audio) {
        const value = statsData.audio[key]
        let valuePretty = value

        if (typeof value == "number" && key.endsWith("Ms")) {
            valuePretty = `${num(value, "ms")}`
        }

        text += `${key}: ${valuePretty}\n`
    }

    return text
}

export class StreamStats {

    private logger: Logger | null = null

    private enabled: boolean = false
    private transport: Transport | null = null
    private statsChannel: DataTransportChannel | null = null
    private updateIntervalId: number | null = null
    private updateInProgress: boolean = false
    private readonly boundOnRawData = this.onRawData.bind(this)

    private benchmarkStartedAt: string | null = null
    private benchmarkStartTimeMs: number | null = null
    private benchmarkSamples: Array<BenchmarkSample> = []
    private benchmarkNextSequence = 0
    private readonly benchmarkSampleLimit = 3600
    private readonly benchmarkContext: BenchmarkContext
    private sourceUpdatedAtMs: Record<
        "transport" |
        "streamerRtt" |
        "streamerVideo" |
        "streamerVideoTransport" |
        "streamerBrowserRtt" |
        "runtimeBitrateControl" |
        "video" |
        "audio",
        number | null
    > = {
        transport: null,
        streamerRtt: null,
        streamerVideo: null,
        streamerVideoTransport: null,
        streamerBrowserRtt: null,
        runtimeBitrateControl: null,
        video: null,
        audio: null,
    }

    private videoPipe: Pipe | null = null
    private audioPipe: Pipe | null = null
    private statsData: StreamStatsData = {
        videoCodec: null,
        videoWidth: null,
        videoHeight: null,
        videoFps: null,
        videoPipeline: null,
        audioPipeline: null,
        hdrEnabled: null,
        streamerRttMs: null,
        streamerRttVarianceMs: null,
        minHostProcessingLatencyMs: null,
        maxHostProcessingLatencyMs: null,
        avgHostProcessingLatencyMs: null,
        minStreamerProcessingTimeMs: null,
        maxStreamerProcessingTimeMs: null,
        avgStreamerProcessingTimeMs: null,
        browserRtt: null,
        transport: {},
        streamerVideoTransport: {},
        runtimeBitrateControl: {},
        video: {},
        audio: {}
    }

    constructor(logger?: Logger, benchmarkContext?: BenchmarkContext) {
        if (logger) {
            this.logger = logger
        }
        this.benchmarkContext = benchmarkContext ?? {
            pageUrl: UNKNOWN,
            userAgent: UNKNOWN,
            streamSettings: {
                bitrateKbps: 0,
                width: 0,
                height: 0,
                fps: 0,
                requestedVideoCodec: UNKNOWN,
                requestedHdr: false,
                requestedDataTransport: UNKNOWN,
                iceTransportPolicy: UNKNOWN,
                playAudioLocal: false,
                videoFrameQueueSize: 0,
                audioSampleQueueSize: 0,
            },
        }
    }

    setTransport(transport: Transport) {
        this.transport = transport

        this.checkEnabled()
    }
    private checkEnabled() {
        if (this.enabled) {
            if (this.statsChannel) {
                this.statsChannel.removeReceiveListener(this.boundOnRawData)
                this.statsChannel = null
            }

            if (!this.statsChannel && this.transport) {
                const channel = this.transport.getChannel(TransportChannelId.STATS)
                if (channel.type != "data") {
                    this.logger?.debug(`Failed initialize debug transport channel because type is "${channel.type}" and not "data"`)
                    return
                }
                channel.addReceiveListener(this.boundOnRawData)
                this.statsChannel = channel
            }
            if (this.updateIntervalId == null) {
                this.updateIntervalId = setInterval(this.updateLocalStats.bind(this), 1000)
            }
        } else {
            if (this.updateIntervalId != null) {
                clearInterval(this.updateIntervalId)
                this.updateIntervalId = null
            }
        }
    }

    setEnabled(enabled: boolean) {
        if (enabled && !this.enabled) {
            this.resetBenchmark()
        }
        this.enabled = enabled

        this.checkEnabled()
    }
    isEnabled(): boolean {
        return this.enabled
    }
    toggle() {
        this.setEnabled(!this.isEnabled())
    }

    private buffer: ByteBuffer = BIG_BUFFER
    private onRawData(data: ArrayBuffer) {
        this.buffer.reset()
        this.buffer.putU8Array(new Uint8Array(data))

        this.buffer.flip()

        const textLength = this.buffer.getU16()
        const text = this.buffer.getUtf8Raw(textLength)

        const json: StreamerStatsUpdate = JSON.parse(text)
        this.onMessage(json)
    }
    private onMessage(msg: StreamerStatsUpdate) {
        if ("Rtt" in msg) {
            this.sourceUpdatedAtMs.streamerRtt = performance.now()
            this.statsData.streamerRttMs = msg.Rtt.rtt_ms
            this.statsData.streamerRttVarianceMs = msg.Rtt.rtt_variance_ms
        } else if ("Video" in msg) {
            this.sourceUpdatedAtMs.streamerVideo = performance.now()
            if (msg.Video.host_processing_latency) {
                this.statsData.minHostProcessingLatencyMs = msg.Video.host_processing_latency.min_host_processing_latency_ms
                this.statsData.maxHostProcessingLatencyMs = msg.Video.host_processing_latency.max_host_processing_latency_ms
                this.statsData.avgHostProcessingLatencyMs = msg.Video.host_processing_latency.avg_host_processing_latency_ms
            } else {
                this.statsData.minHostProcessingLatencyMs = null
                this.statsData.maxHostProcessingLatencyMs = null
                this.statsData.avgHostProcessingLatencyMs = null
            }

            this.statsData.minStreamerProcessingTimeMs = msg.Video.min_streamer_processing_time_ms
            this.statsData.maxStreamerProcessingTimeMs = msg.Video.max_streamer_processing_time_ms
            this.statsData.avgStreamerProcessingTimeMs = msg.Video.avg_streamer_processing_time_ms
        } else if ("VideoTransport" in msg) {
            this.sourceUpdatedAtMs.streamerVideoTransport = performance.now()
            const source = msg.VideoTransport
            const stats: Record<string, StatValue> = {
                intervalMs: source.interval_ms,
                queueCapacityFrames: Number(source.queue_capacity_frames),
                queueDepthFrames: Number(source.queue_depth_frames),
                queueMaxDepthFrames: Number(source.queue_max_depth_frames),
                inFlightFrames: Number(source.in_flight_frames),
                inFlightMaxFrames: Number(source.in_flight_max_frames),
                encodedFramesReceived: Number(source.encoded_frames_received),
                encodedPayloadBytesReceived: Number(source.encoded_payload_bytes_received),
                framesAccepted: Number(source.frames_accepted),
                framesRejected: Number(source.frames_rejected),
                framesReplaced: Number(source.frames_replaced),
                framesCleared: Number(source.frames_cleared),
                framesDropped: Number(source.frames_dropped),
                framesDequeued: Number(source.frames_dequeued),
                idrFramesReceived: Number(source.idr_frames_received),
                idrEncodedPayloadBytesReceived: Number(source.idr_encoded_payload_bytes_received),
                idrFramesAccepted: Number(source.idr_frames_accepted),
                idrRtpPayloadBytesAccepted: Number(source.idr_rtp_payload_bytes_accepted),
                rtpPacketsDequeued: Number(source.rtp_packets_dequeued),
                rtpPayloadBytesDequeued: Number(source.rtp_payload_bytes_dequeued),
                rtpPacketsWriteSucceeded: Number(source.rtp_packets_write_succeeded),
                rtpPayloadBytesWriteSucceeded: Number(source.rtp_payload_bytes_write_succeeded),
                rtpPacketsWriteFailed: Number(source.rtp_packets_write_failed),
                rtpPayloadBytesWriteFailed: Number(source.rtp_payload_bytes_write_failed),
                rtpPacketsWriteSkipped: Number(source.rtp_packets_write_skipped),
                rtpPayloadBytesWriteSkipped: Number(source.rtp_payload_bytes_write_skipped),
                queueWaitSamples: Number(source.queue_wait_samples),
                queueWaitMinMs: source.queue_wait_min_ms,
                queueWaitMaxMs: source.queue_wait_max_ms,
                queueWaitAvgMs: source.queue_wait_avg_ms,
                rtpWriteLatencySamples: Number(source.rtp_write_latency_samples),
                rtpWriteLatencyMinMs: source.rtp_write_latency_min_ms,
                rtpWriteLatencyMaxMs: source.rtp_write_latency_max_ms,
                rtpWriteLatencyAvgMs: source.rtp_write_latency_avg_ms,
            }

            if (Number.isFinite(source.interval_ms) && source.interval_ms > 0) {
                // bits / millisecond is numerically equal to decimal kbit/s.
                stats.encodedInputBitrateKbps = Number(source.encoded_payload_bytes_received) * 8 / source.interval_ms
                stats.rtpDequeuedPayloadBitrateKbps = Number(source.rtp_payload_bytes_dequeued) * 8 / source.interval_ms
                stats.rtpWriteSucceededPayloadBitrateKbps = Number(source.rtp_payload_bytes_write_succeeded) * 8 / source.interval_ms
            }

            this.statsData.streamerVideoTransport = stats
        } else if ("BrowserRtt" in msg) {
            this.sourceUpdatedAtMs.streamerBrowserRtt = performance.now()
            this.statsData.browserRtt = msg.BrowserRtt.rtt_ms
        } else if ("RuntimeBitrateControl" in msg) {
            this.sourceUpdatedAtMs.runtimeBitrateControl = performance.now()
            const source = msg.RuntimeBitrateControl
            this.statsData.runtimeBitrateControl = {
                targetKbps: Number(source.target_kbps),
                requestedKbps: Number(source.requested_kbps),
                state: source.state,
                attempts: Number(source.attempts),
            }
        }
    }

    private async updateLocalStats() {
        if (this.updateInProgress) {
            return
        }

        this.updateInProgress = true
        try {
            await Promise.all([
                this.updateTransportStats(),
                this.updateVideoStats(),
                this.updateAudioStats(),
            ])
            this.captureBenchmarkSample()
        } finally {
            this.updateInProgress = false
        }
    }
    private async updateTransportStats() {
        if (!this.transport) {
            console.debug("Cannot query stats without transport")
            return
        }

        // Transport stats contain both instantaneous values and interval deltas.
        // Replace the snapshot so a missing value cannot be mistaken for a fresh
        // sample from the current interval.
        this.statsData.transport = await this.transport.getStats()
        this.sourceUpdatedAtMs.transport = performance.now()
    }
    private async updateVideoStats() {
        const stats = {}

        if (this.videoPipe && this.videoPipe.reportStats) {
            this.videoPipe.reportStats(stats)
        }

        this.statsData.video = stats
        this.sourceUpdatedAtMs.video = performance.now()
    }
    private async updateAudioStats() {
        const stats = {}

        if (this.audioPipe && this.audioPipe.reportStats) {
            this.audioPipe.reportStats(stats)
        }

        this.statsData.audio = stats
        this.sourceUpdatedAtMs.audio = performance.now()
    }

    setVideoInfo(codec: string, width: number, height: number, fps: number) {
        this.statsData.videoCodec = codec
        this.statsData.videoWidth = width
        this.statsData.videoHeight = height
        this.statsData.videoFps = fps
    }
    setVideoPipeline(name: string, pipe: Pipe | null) {
        this.statsData.videoPipeline = name
        this.videoPipe = pipe
    }
    setAudioPipeline(name: string, pipe: Pipe | null) {
        this.statsData.audioPipeline = name
        this.audioPipe = pipe
    }
    setHdrEnabled(enabled: boolean) {
        this.statsData.hdrEnabled = enabled
    }

    getCurrentStats(): StreamStatsData {
        return {
            ...this.statsData,
            transport: { ...this.statsData.transport },
            streamerVideoTransport: { ...this.statsData.streamerVideoTransport },
            runtimeBitrateControl: { ...this.statsData.runtimeBitrateControl },
            video: { ...this.statsData.video },
            audio: { ...this.statsData.audio },
        }
    }

    resetBenchmark() {
        this.benchmarkStartedAt = new Date().toISOString()
        this.benchmarkStartTimeMs = performance.now()
        this.benchmarkSamples = []
        this.benchmarkNextSequence = 0
    }

    private captureBenchmarkSample() {
        if (!this.enabled || this.benchmarkStartTimeMs == null) {
            return
        }

        const capturedAtMs = performance.now()
        const freshnessMs = (source: keyof typeof this.sourceUpdatedAtMs): number | null => {
            const updatedAtMs = this.sourceUpdatedAtMs[source]
            return updatedAtMs == null ? null : Math.max(0, capturedAtMs - updatedAtMs)
        }

        this.benchmarkSamples.push({
            sequence: this.benchmarkNextSequence++,
            elapsedMs: capturedAtMs - this.benchmarkStartTimeMs,
            collectedAt: new Date().toISOString(),
            freshnessMs: {
                transport: freshnessMs("transport"),
                streamerRtt: freshnessMs("streamerRtt"),
                streamerVideo: freshnessMs("streamerVideo"),
                streamerVideoTransport: freshnessMs("streamerVideoTransport"),
                streamerBrowserRtt: freshnessMs("streamerBrowserRtt"),
                runtimeBitrateControl: freshnessMs("runtimeBitrateControl"),
                video: freshnessMs("video"),
                audio: freshnessMs("audio"),
            },
            stats: this.getCurrentStats(),
        })

        if (this.benchmarkSamples.length > this.benchmarkSampleLimit) {
            this.benchmarkSamples.shift()
        }
    }

    getBenchmarkReport(): BenchmarkReport {
        const exportedAt = new Date().toISOString()
        const firstSample = this.benchmarkSamples[0]
        const lastSample = this.benchmarkSamples[this.benchmarkSamples.length - 1]

        return {
            schemaVersion: 2,
            startedAt: this.benchmarkStartedAt,
            endedAt: exportedAt,
            exportedAt,
            sampleWindowStartedAt: firstSample?.collectedAt ?? null,
            sampleWindowEndedAt: lastSample?.collectedAt ?? null,
            application: {
                name: "betterparsec",
                version: UNKNOWN,
                build: UNKNOWN,
                gitSha: UNKNOWN,
            },
            host: {
                softwareVersion: UNKNOWN,
                gpuDriverVersion: UNKNOWN,
                gitSha: UNKNOWN,
            },
            browser: {
                userAgent: this.benchmarkContext.userAgent || UNKNOWN,
            },
            page: {
                url: sanitizeBenchmarkPageUrl(this.benchmarkContext.pageUrl),
                queryParametersIncluded: false,
            },
            streamSettings: { ...this.benchmarkContext.streamSettings },
            selected: {
                videoCodec: this.statsData.videoCodec,
                videoPipeline: this.statsData.videoPipeline,
                audioPipeline: this.statsData.audioPipeline,
                hdrEnabled: this.statsData.hdrEnabled,
            },
            sampleSequence: {
                first: firstSample?.sequence ?? null,
                last: lastSample?.sequence ?? null,
                count: this.benchmarkSamples.length,
                retainedLimit: this.benchmarkSampleLimit,
            },
            samples: this.benchmarkSamples.map(sample => ({
                ...sample,
                freshnessMs: { ...sample.freshnessMs },
                stats: {
                    ...sample.stats,
                    transport: { ...sample.stats.transport },
                    streamerVideoTransport: { ...sample.stats.streamerVideoTransport },
                    runtimeBitrateControl: { ...sample.stats.runtimeBitrateControl },
                    video: { ...sample.stats.video },
                    audio: { ...sample.stats.audio },
                },
            })),
            limitations: [
                "Application version/build/git SHA are unknown because build metadata is not injected into the browser bundle.",
                "Host software version, GPU driver version, and host git SHA are unknown to the browser.",
                "Page query parameters and fragments are omitted because they may contain host, app, or session identifiers.",
                "Freshness values are browser-side ages since the last received or collected source update, not source-generation timestamps.",
                "RTP payload/header bytes are not full wire bytes.",
                "Media timestamps do not provide input-to-photon latency or absolute frame age.",
                "Optional WebRTC fields vary by browser and may be absent.",
            ],
        }
    }
}
