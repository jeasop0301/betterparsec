import { StreamSignalingMessage, TransportChannelId } from "../../api_bindings.js";
import { Logger } from "../log.js";
import { StatValue } from "../stats.js";
import { CAPABILITIES_CODECS, emptyVideoCodecs, maybeVideoCodecs, VideoCodecSupport } from "../video.js";
import { DataTransportChannel, Transport, TransportAudioSetup, TransportChannel, TransportChannelIdKey, TransportChannelIdValue, TransportVideoSetup, AudioTrackTransportChannel, VideoTrackTransportChannel, TrackTransportChannel, TransportShutdown } from "./index.js";
import { CounterInterval, CumulativeAverageSampler, CumulativeCounterSampler, extractSelectedIceCandidatePairStats } from "./stats_sampler.js";

export class WebRTCTransport implements Transport {
    implementationName: string = "webrtc"

    private logger: Logger | null

    private peer: RTCPeerConnection | null = null
    private cumulativeAverages = new CumulativeAverageSampler()
    private cumulativeCounters = new CumulativeCounterSampler()

    private sampleAverageMilliseconds(
        statsId: string,
        metric: string,
        totalSeconds: number,
        count: number,
        timestampMs: number,
    ): number | null {
        return this.cumulativeAverages.sampleMilliseconds(
            `${statsId}:${metric}`,
            totalSeconds,
            count,
            timestampMs,
        )
    }

    private addCumulativeCounter(
        statsData: Record<string, StatValue>,
        statsId: string,
        metric: string,
        value: number,
        timestampMs: number,
        totalDestination: string,
        deltaDestination: string,
    ): CounterInterval | null {
        if (!Number.isFinite(value) || !Number.isFinite(timestampMs)) {
            return null
        }

        statsData[totalDestination] = value

        const interval = this.cumulativeCounters.sample(
            `${statsId}:${metric}`,
            value,
            timestampMs,
        )
        if (interval) {
            statsData[deltaDestination] = interval.delta
            if (!("webrtcStatsIntervalMs" in statsData)) {
                statsData.webrtcStatsIntervalMs = interval.elapsedMs
            }
        }

        return interval
    }

    constructor(logger?: Logger) {
        this.logger = logger ?? null
    }

    async initPeer(configuration?: RTCConfiguration) {
        this.logger?.debug(`Creating Client Peer`)

        if (this.peer) {
            this.logger?.debug(`Cannot create Peer because a Peer already exists`)
            return
        }

        // Configure web rtc
        // TODO: use this for signaling instead and extend the protocol so that the client also requests a control channel with name: "control", protocol:"moonlight-control-v1": https://www.ietf.org/archive/id/draft-ietf-wish-whep-02.html
        this.peer = new RTCPeerConnection(configuration)
        this.peer.addEventListener("error", this.onError.bind(this))

        this.peer.addEventListener("icecandidate", this.onIceCandidate.bind(this))

        this.peer.addEventListener("connectionstatechange", this.onConnectionStateChange.bind(this))
        this.peer.addEventListener("signalingstatechange", this.onSignalingStateChange.bind(this))
        this.peer.addEventListener("iceconnectionstatechange", this.onIceConnectionStateChange.bind(this))
        this.peer.addEventListener("icegatheringstatechange", this.onIceGatheringStateChange.bind(this))

        this.peer.addEventListener("track", this.onTrack.bind(this))
        this.peer.addEventListener("datachannel", this.onDataChannel.bind(this))

        this.initChannels()

        // Maybe we already received data
        if (this.remoteDescription) {
            await this.handleRemoteDescription(this.remoteDescription)
        }
        await this.tryDequeueIceCandidates()
    }

    private onError(event: Event) {
        this.logger?.debug(`Web Socket or WebRtcPeer Error`)

        console.error(`Web Socket or WebRtcPeer Error`, event)
    }

    onsendmessage: ((message: StreamSignalingMessage) => void) | null = null
    private sendMessage(message: StreamSignalingMessage) {
        if (this.onsendmessage) {
            this.onsendmessage(message)
        } else {
            this.logger?.debug("Failed to call onicecandidate because no handler is set")
        }
    }
    async onReceiveMessage(message: StreamSignalingMessage) {
        if ("Description" in message) {
            const description = message.Description;
            await this.handleRemoteDescription({
                type: description.ty as RTCSdpType,
                sdp: description.sdp
            })
        } else if ("AddIceCandidate" in message) {
            const candidate = message.AddIceCandidate
            await this.addIceCandidate({
                candidate: candidate.candidate,
                sdpMid: candidate.sdp_mid,
                sdpMLineIndex: candidate.sdp_mline_index,
                usernameFragment: candidate.username_fragment
            })
        }
    }

    private remoteDescription: RTCSessionDescriptionInit | null = null
    private async handleRemoteDescription(sdp: RTCSessionDescriptionInit | null) {
        this.logger?.debug(`Received remote description: ${sdp?.type}`)

        const remoteDescription = sdp
        this.remoteDescription = remoteDescription
        if (!this.peer) {
            return
        }
        this.remoteDescription = null

        if (remoteDescription) {
            await this.peer.setRemoteDescription(remoteDescription)

            if (remoteDescription.type == "offer") {
                await this.peer.setLocalDescription()
                const localDescription = this.peer.localDescription
                if (!localDescription) {
                    this.logger?.debug("Peer didn't have a localDescription whilst receiving an offer and trying to answer")
                    return
                }

                this.logger?.debug(`Responding to offer description: ${localDescription.type}`)
                this.sendMessage({
                    Description: {
                        ty: localDescription.type,
                        sdp: localDescription.sdp ?? ""
                    }
                })
            }
        }
    }

    private onIceCandidate(event: RTCPeerConnectionIceEvent) {
        if (event.candidate) {
            const candidate = event.candidate.toJSON()
            this.logger?.debug(`Sending ice candidate: ${candidate.candidate}`)

            this.sendMessage({
                AddIceCandidate: {
                    candidate: candidate.candidate ?? "",
                    sdp_mid: candidate.sdpMid ?? null,
                    sdp_mline_index: candidate.sdpMLineIndex ?? null,
                    username_fragment: candidate.usernameFragment ?? null
                }
            })
        } else {
            this.logger?.debug("No new ice candidates")
        }
    }

    private iceCandidates: Array<RTCIceCandidateInit> = []
    private async addIceCandidate(candidate: RTCIceCandidateInit) {
        this.logger?.debug(`Received ice candidate: ${candidate.candidate}`)

        if (!this.peer) {
            this.logger?.debug("Buffering ice candidate")

            this.iceCandidates.push(candidate)
            return
        }
        await this.tryDequeueIceCandidates()

        await this.peer.addIceCandidate(candidate)
    }
    private async tryDequeueIceCandidates() {
        if (!this.peer) {
            this.logger?.debug("called tryDequeueIceCandidates without a peer")
            return
        }

        for (const candidate of this.iceCandidates) {
            await this.peer.addIceCandidate(candidate)
        }
        this.iceCandidates.length = 0
    }

    private wasConnected = false
    private onConnectionStateChange() {
        if (!this.peer) {
            this.logger?.debug("OnConnectionStateChange without a peer")
            return
        }

        let type: null | "fatal" | "recover" = null

        if (this.peer.connectionState == "connected") {
            type = "recover"

            if (this.onconnect) {
                this.onconnect()
            }
            this.wasConnected = true
        } else if ((this.peer.connectionState == "failed" || this.peer.connectionState == "closed") && this.peer.iceGatheringState == "complete") {
            type = "fatal"
        }

        if (this.peer.connectionState == "failed" || this.peer.connectionState == "closed") {
            if (this.onclose) {
                if (this.wasConnected) {
                    this.onclose("failed")
                } else {
                    this.onclose("failednoconnect")
                }
            }
        }

        this.logger?.debug(`Changing Peer State to ${this.peer.connectionState}`, {
            type: type ?? undefined
        })
    }
    private onSignalingStateChange() {
        if (!this.peer) {
            this.logger?.debug("OnSignalingStateChange without a peer")
            return
        }
        this.logger?.debug(`Changing Peer Signaling State to ${this.peer.signalingState}`)
    }
    private onIceConnectionStateChange() {
        if (!this.peer) {
            this.logger?.debug("OnIceConnectionStateChange without a peer")
            return
        }
        this.logger?.debug(`Changing Peer Ice State to ${this.peer.iceConnectionState}`)
    }
    private onIceGatheringStateChange() {
        if (!this.peer) {
            this.logger?.debug("OnIceGatheringStateChange without a peer")
            return
        }
        this.logger?.debug(`Changing Peer Ice Gathering State to ${this.peer.iceGatheringState}`)

        if (this.peer.iceConnectionState == "new" && this.peer.iceGatheringState == "complete") {
            // we failed without connection
            if (this.onclose) {
                this.onclose("failednoconnect")
            }
        }
    }

    private channels: Array<TransportChannel | null> = []
    private initChannels() {
        if (!this.peer) {
            this.logger?.debug("Failed to initialize channel without peer")
            return
        }
        if (this.channels.length > 0) {
            this.logger?.debug("Already initialized channels")
            return
        }

        for (const channelRaw in TransportChannelId) {
            const channel = channelRaw as TransportChannelIdKey

            if (channel == "HOST_VIDEO") {
                const channel: VideoTrackTransportChannel = new WebRTCInboundTrackTransportChannel<"videotrack">(this.logger, "videotrack", "video", this.videoTrackHolder)
                this.channels[TransportChannelId.HOST_VIDEO] = channel
                continue
            }
            if (channel == "HOST_AUDIO") {
                const channel: AudioTrackTransportChannel = new WebRTCInboundTrackTransportChannel<"audiotrack">(this.logger, "audiotrack", "audio", this.audioTrackHolder)
                this.channels[TransportChannelId.HOST_AUDIO] = channel
                continue
            }

            // All Data Channels are created by the server
            const id = TransportChannelId[channel]
            this.channels[id] = new WebRTCDataTransportChannel(channel, null)
        }
    }

    private videoTrackHolder: TrackHolder = { ontrack: null, track: null }
    private videoReceiver: RTCRtpReceiver | null = null

    private audioTrackHolder: TrackHolder = { ontrack: null, track: null }

    private onTrack(event: RTCTrackEvent) {
        const track = event.track

        const receiver = event.receiver
        if (track.kind == "video") {
            this.videoReceiver = receiver
        }

        receiver.jitterBufferTarget = 0
        if ("playoutDelayHint" in receiver) {
            receiver.playoutDelayHint = 0
        }

        this.logger?.debug(`Adding receiver: ${track.kind}, ${track.id}, ${track.label}`)

        if (track.kind == "video") {
            if ("contentHint" in track) {
                track.contentHint = "motion"
            }

            this.videoTrackHolder.track = track
            if (!this.videoTrackHolder.ontrack) {
                throw "No video track listener registered!"
            }
            this.videoTrackHolder.ontrack()
        } else if (track.kind == "audio") {
            this.audioTrackHolder.track = track
            if (!this.audioTrackHolder.ontrack) {
                throw "No audio track listener registered!"
            }
            this.audioTrackHolder.ontrack()
        }
    }

    // Handle data channels created by the remote peer (server)
    private onDataChannel(event: RTCDataChannelEvent) {
        const remoteChannel = event.channel
        const label = remoteChannel.label

        this.logger?.debug(`Received remote data channel: ${label}`)

        // Map the channel label to the corresponding TransportChannelId
        const channelKey = label.toUpperCase() as TransportChannelIdKey
        if (channelKey in TransportChannelId) {
            const id = TransportChannelId[channelKey]
            const existingChannel = this.channels[id]

            // If we already have a channel for this ID, replace its underlying RTCDataChannel
            // with the remote one so we can receive messages from the server
            if (existingChannel && existingChannel.type === "data") {
                this.logger?.debug(`Replacing underlying channel for ${label} with remote channel`);
                (existingChannel as WebRTCDataTransportChannel).replaceChannel(remoteChannel)
            } else {
                this.logger?.debug(`Creating new channel for ${label}`)
                this.channels[id] = new WebRTCDataTransportChannel(label, remoteChannel)
            }
        } else {
            this.logger?.debug(`Unknown remote data channel: ${label}`)
        }
    }

    async setupHostVideo(_setup: TransportVideoSetup): Promise<VideoCodecSupport> {
        // TODO: check transport type

        let capabilities
        if ("getCapabilities" in RTCRtpReceiver && (capabilities = RTCRtpReceiver.getCapabilities("video"))) {
            const codecs = emptyVideoCodecs()

            for (const codec in codecs) {
                const supportRequirements = CAPABILITIES_CODECS[codec]

                if (!supportRequirements) {
                    continue
                }

                let supported = false
                capabilityCodecLoop: for (const codecCapability of capabilities.codecs) {
                    if (codecCapability.mimeType != supportRequirements.mimeType) {
                        continue
                    }

                    for (const fmtpLine of supportRequirements.fmtpLine) {
                        if (!codecCapability.sdpFmtpLine?.includes(fmtpLine)) {
                            continue capabilityCodecLoop
                        }
                    }

                    supported = true
                    break
                }

                codecs[codec] = supported
            }

            return codecs
        } else {
            return maybeVideoCodecs()
        }
    }

    async setupHostAudio(_setup: TransportAudioSetup): Promise<void> {
        // TODO: check transport type
    }

    getChannel(id: TransportChannelIdValue): TransportChannel {
        const channel = this.channels[id]
        if (!channel) {
            this.logger?.debug("Failed to setup video without peer")
            throw `Failed to get channel because it is not yet initialized, Id: ${id}`
        }

        return channel
    }

    onconnect: (() => void) | null = null

    onclose: ((shutdown: TransportShutdown) => void) | null = null
    async close(): Promise<void> {
        this.logger?.debug("Closing WebRTC Peer")

        this.cumulativeAverages.clear()
        this.cumulativeCounters.clear()
        this.peer?.close()
    }

    async getStats(): Promise<Record<string, StatValue>> {
        const statsData: Record<string, StatValue> = {}

        if (!this.videoReceiver) {
            return {}
        }
        const stats = await this.videoReceiver.getStats()

        let iceStats = extractSelectedIceCandidatePairStats(stats.entries())
        if (!("webrtcSelectedCandidatePairId" in iceStats) && this.peer) {
            // Receiver-scoped reports do not consistently include candidate
            // dictionaries across browsers. Fall back to the complete peer
            // report only when the selected path could not be proven.
            const peerStats = await this.peer.getStats()
            iceStats = extractSelectedIceCandidatePairStats(peerStats.entries())
        }
        Object.assign(statsData, iceStats)

        for (const [key, value] of stats.entries()) {
            if ("decoderImplementation" in value && value.decoderImplementation != null) {
                statsData.decoderImplementation = value.decoderImplementation
            }
            if ("frameWidth" in value && value.frameWidth != null) {
                statsData.videoWidth = value.frameWidth
            }
            if ("frameHeight" in value && value.frameHeight != null) {
                statsData.videoHeight = value.frameHeight
            }
            if ("framesPerSecond" in value && value.framesPerSecond != null) {
                statsData.webrtcFps = value.framesPerSecond
            }

            const statsId = typeof value.id == "string" ? value.id : key
            const timestampMs = value.timestamp
            if (typeof timestampMs != "number" || !Number.isFinite(timestampMs)) {
                continue
            }
            const mediaKind = value.kind ?? value.mediaType
            if (value.type != "inbound-rtp" || (mediaKind != null && mediaKind != "video")) {
                // receiver.getStats() also includes transport and candidate-pair
                // dictionaries. Their byte counters have different semantics and
                // must not overwrite the inbound-video RTP counters below.
                continue
            }
            const jitterBufferEmittedCount = value.jitterBufferEmittedCount
            if (typeof jitterBufferEmittedCount == "number") {
                const jitterBufferMetrics = [
                    ["jitterBufferDelay", "webrtcJitterBufferDelayMs"],
                    ["jitterBufferTargetDelay", "webrtcJitterBufferTargetDelayMs"],
                    ["jitterBufferMinimumDelay", "webrtcJitterBufferMinimumDelayMs"],
                ] as const

                for (const [source, destination] of jitterBufferMetrics) {
                    const totalSeconds = value[source]
                    if (typeof totalSeconds == "number") {
                        const averageMs = this.sampleAverageMilliseconds(
                            statsId,
                            source,
                            totalSeconds,
                            jitterBufferEmittedCount,
                            timestampMs,
                        )
                        if (averageMs != null) {
                            statsData[destination] = averageMs
                        }
                    }
                }
            }
            if (typeof value.jitter == "number") {
                // RTCInboundRtpStreamStats.jitter is an instantaneous value in seconds.
                statsData.webrtcJitterMs = value.jitter * 1000
            }

            const framesDecoded = value.framesDecoded
            if (typeof framesDecoded == "number") {
                const frameMetrics = [
                    ["totalDecodeTime", "webrtcAvgDecodeTimeMs"],
                    ["totalProcessingDelay", "webrtcAvgProcessingDelayMs"],
                ] as const

                for (const [source, destination] of frameMetrics) {
                    const totalSeconds = value[source]
                    if (typeof totalSeconds == "number") {
                        const averageMs = this.sampleAverageMilliseconds(
                            statsId,
                            source,
                            totalSeconds,
                            framesDecoded,
                            timestampMs,
                        )
                        if (averageMs != null) {
                            statsData[destination] = averageMs
                        }
                    }
                }
            }

            if (typeof value.totalAssemblyTime == "number"
                && typeof value.framesAssembledFromMultiplePackets == "number") {
                const averageMs = this.sampleAverageMilliseconds(
                    statsId,
                    "totalAssemblyTime",
                    value.totalAssemblyTime,
                    value.framesAssembledFromMultiplePackets,
                    timestampMs,
                )
                if (averageMs != null) {
                    statsData.webrtcAvgAssemblyTimeMs = averageMs
                }
            }
            let packetsReceivedInterval: CounterInterval | null = null
            if (typeof value.packetsReceived == "number") {
                packetsReceivedInterval = this.addCumulativeCounter(
                    statsData,
                    statsId,
                    "packetsReceived",
                    value.packetsReceived,
                    timestampMs,
                    "webrtcPacketsReceived",
                    "webrtcPacketsReceivedDelta",
                )
            }

            let packetsLostInterval: CounterInterval | null = null
            if (typeof value.packetsLost == "number") {
                // packetsLost is signed and can decrease when late packets arrive.
                // The sampler treats a decrease as a new baseline instead of
                // reporting a misleading negative interval loss rate.
                packetsLostInterval = this.addCumulativeCounter(
                    statsData,
                    statsId,
                    "packetsLost",
                    value.packetsLost,
                    timestampMs,
                    "webrtcPacketsLost",
                    "webrtcPacketsLostDelta",
                )
            }

            if (packetsReceivedInterval && packetsLostInterval) {
                const packetOutcomeDelta = packetsReceivedInterval.delta + packetsLostInterval.delta
                if (packetOutcomeDelta > 0) {
                    statsData.webrtcPacketLossPercent = packetsLostInterval.delta * 100 / packetOutcomeDelta
                }
            }

            const cumulativeFrameCounters = [
                ["framesReceived", "webrtcFramesReceived", "webrtcFramesReceivedDelta"],
                ["framesDecoded", "webrtcFramesDecoded", "webrtcFramesDecodedDelta"],
                ["framesRendered", "webrtcFramesRendered", "webrtcFramesRenderedDelta"],
                ["framesDropped", "webrtcFramesDropped", "webrtcFramesDroppedDelta"],
                ["keyFramesDecoded", "webrtcKeyFramesDecoded", "webrtcKeyFramesDecodedDelta"],
            ] as const

            for (const [source, totalDestination, deltaDestination] of cumulativeFrameCounters) {
                const total = value[source]
                if (typeof total == "number") {
                    this.addCumulativeCounter(
                        statsData,
                        statsId,
                        source,
                        total,
                        timestampMs,
                        totalDestination,
                        deltaDestination,
                    )
                }
            }

            const feedbackCounters = [
                ["nackCount", "webrtcNackCount", "webrtcNackCountDelta"],
                ["pliCount", "webrtcPliCount", "webrtcPliCountDelta"],
                ["firCount", "webrtcFirCount", "webrtcFirCountDelta"],
                ["freezeCount", "webrtcFreezeCount", "webrtcFreezeCountDelta"],
            ] as const

            for (const [source, totalDestination, deltaDestination] of feedbackCounters) {
                const total = value[source]
                if (typeof total == "number") {
                    this.addCumulativeCounter(
                        statsData,
                        statsId,
                        source,
                        total,
                        timestampMs,
                        totalDestination,
                        deltaDestination,
                    )
                }
            }

            if (typeof value.totalFreezesDuration == "number") {
                statsData.webrtcTotalFreezesDurationMs = value.totalFreezesDuration * 1000
                const interval = this.cumulativeCounters.sample(
                    `${statsId}:totalFreezesDuration`,
                    value.totalFreezesDuration,
                    timestampMs,
                )
                if (interval) {
                    statsData.webrtcFreezesDurationDeltaMs = interval.delta * 1000
                }
            }

            if (typeof value.bytesReceived == "number") {
                // RTCInboundRtpStreamStats.bytesReceived excludes RTP headers,
                // padding, and lower-layer overhead, so this is payload bitrate,
                // not on-the-wire bitrate.
                const interval = this.addCumulativeCounter(
                    statsData,
                    statsId,
                    "bytesReceived",
                    value.bytesReceived,
                    timestampMs,
                    "webrtcPayloadBytesReceived",
                    "webrtcPayloadBytesReceivedDelta",
                )
                if (interval) {
                    // bits / millisecond is numerically equal to decimal kbit/s.
                    statsData.webrtcPayloadReceiveBitrateKbps = interval.delta * 8 / interval.elapsedMs
                }
            }

            if (typeof value.headerBytesReceived == "number") {
                const interval = this.addCumulativeCounter(
                    statsData,
                    statsId,
                    "headerBytesReceived",
                    value.headerBytesReceived,
                    timestampMs,
                    "webrtcRtpHeaderBytesReceived",
                    "webrtcRtpHeaderBytesReceivedDelta",
                )
                if (interval) {
                    statsData.webrtcRtpHeaderReceiveBitrateKbps = interval.delta * 8 / interval.elapsedMs
                }
            }
        }

        return statsData
    }
}

type TrackHolder = {
    ontrack: (() => void) | null
    track: MediaStreamTrack | null
}

// This receives track data
class WebRTCInboundTrackTransportChannel<T extends string> implements TrackTransportChannel {
    type: T

    canReceive: boolean = true
    canSend: boolean = false

    private logger: Logger | null

    private label: string
    private trackHolder: TrackHolder

    constructor(logger: Logger | null, type: T, label: string, trackHolder: TrackHolder) {
        this.logger = logger

        this.type = type
        this.label = label
        this.trackHolder = trackHolder

        this.trackHolder.ontrack = this.onTrack.bind(this)
    }
    setTrack(_track: MediaStreamTrack | null): void {
        throw "WebRTCInboundTrackTransportChannel cannot addTrack"
    }

    private onTrack() {
        const track = this.trackHolder.track
        if (!track) {
            this.logger?.debug("WebRTC TrackHolder.track is null!")
            return
        }

        for (const listener of this.trackListeners) {
            listener(track)
        }
    }


    private trackListeners: Array<(track: MediaStreamTrack) => void> = []
    addTrackListener(listener: (track: MediaStreamTrack) => void): void {
        if (this.trackHolder.track) {
            listener(this.trackHolder.track)
        }
        this.trackListeners.push(listener)
    }
    removeTrackListener(listener: (track: MediaStreamTrack) => void): void {
        const index = this.trackListeners.indexOf(listener)
        if (index != -1) {
            this.trackListeners.splice(index, 1)
        }
    }
}

class WebRTCDataTransportChannel implements DataTransportChannel {
    type: "data" = "data"

    canReceive: boolean = true
    canSend: boolean = true

    private logger: Logger | null = null

    private label: string
    private channel: RTCDataChannel | null
    private boundTryDequeueSendQueue: () => void
    private boundOnMessage: (event: MessageEvent) => void

    constructor(label: string, channel: RTCDataChannel | null, logger?: Logger) {
        this.label = label
        this.channel = channel
        this.boundOnMessage = this.onMessage.bind(this)
        this.boundTryDequeueSendQueue = this.tryDequeueSendQueue.bind(this)

        this.logger = logger ?? null

        this.channel?.addEventListener("message", this.boundOnMessage)
    }

    // Replace the underlying channel with a new one (e.g., from remote peer)
    // This is used when we receive a data channel from the server that should
    // replace our locally created one for receiving messages
    replaceChannel(newChannel: RTCDataChannel): void {
        // Remove listener from old channel
        this.channel?.removeEventListener("open", this.boundTryDequeueSendQueue)
        this.channel?.removeEventListener("message", this.boundOnMessage)
        // Add listener to new channel
        this.channel = newChannel
        this.channel.addEventListener("open", this.boundTryDequeueSendQueue)
        this.channel.addEventListener("message", this.boundOnMessage)
    }

    private sendQueue: Array<ArrayBuffer> = []
    send(message: ArrayBuffer): void {
        console.debug(this.label, message)

        if (!this.channel || this.channel.readyState != "open") {
            console.debug(`Tried sending packet to ${this.label} with readyState ${this.channel?.readyState}. Buffering it for the future.`)
            // Make sure to copy the message
            this.sendQueue.push(message.slice(0))
        } else {
            this.channel.send(message)
        }
    }
    private tryDequeueSendQueue() {
        if (!this.channel || this.channel.readyState != "open") {
            return
        }

        for (const message of this.sendQueue.splice(0)) {
            this.channel.send(message)
        }
    }

    private onMessage(event: MessageEvent) {
        const data = event.data
        if (!(data instanceof ArrayBuffer)) {
            console.warn(`received text data on webrtc channel ${this.label}`)
            return
        }

        for (const listener of this.receiveListeners) {
            listener(event.data)
        }
    }
    private receiveListeners: Array<(data: ArrayBuffer) => void> = []
    addReceiveListener(listener: (data: ArrayBuffer) => void): void {
        this.receiveListeners.push(listener)
    }
    removeReceiveListener(listener: (data: ArrayBuffer) => void): void {
        const index = this.receiveListeners.indexOf(listener)
        if (index != -1) {
            this.receiveListeners.splice(index, 1)
        }
    }
    estimatedBufferedBytes(): number | null {
        return this.channel?.bufferedAmount ?? null
    }
}
