import { Api } from "../api.js"
import { App, ConnectionStatus, GeneralClientMessage, GeneralServerMessage, StreamCapabilities, StreamClientMessage, StreamPermissions, StreamServerMessage, StreamSettings, TransportChannelId } from "../api_bindings.js"
import { showNotification } from "../component/notification.js"
import { Component } from "../component/index.js"
import { Settings, TransportType } from "../component/settings_menu.js"
import { AudioPlayer } from "./audio/index.js"
import { buildAudioPipeline } from "./audio/pipeline.js"
import { BIG_BUFFER, ByteBuffer } from "./buffer.js"
import { defaultStreamInputConfig, MouseMode, StreamInput } from "./input.js"
import { CursorAutoAction, CursorAutoMode } from "./cursor_auto.js"
import { parseCursorMessage } from "./cursor_wire.js"
import { Logger, LogMessageInfo } from "./log.js"
import { gatherPipeInfo } from "./pipeline/index.js"
import { BenchmarkContext, StreamStats } from "./stats.js"
import { StallWatchdog, WatchdogAction } from "./session_ux.js"
import { Transport, TransportShutdown } from "./transport/index.js"
import { WebSocketTransport } from "./transport/web_socket.js"
import { WebRTCTransport } from "./transport/webrtc.js"
import { allVideoCodecs, andVideoCodecs, createSupportedVideoFormatsBits, emptyVideoCodecs, getSelectedVideoCodec, hasAnyCodec, VideoCodecSupport } from "./video.js"
import { VideoRenderer } from "./video/index.js"
import { buildFecVideoPipeline, buildVideoPipeline, VideoPipelineOptions } from "./video/pipeline.js"
import { FecDecodePipe } from "./video/fec_decode_pipe.js"
import { encodeAck, SUBSCRIBE_MESSAGE, NEEDS_IDR_MESSAGE } from "./video/fec_wire.js"
import { QuOverlayDom } from "./video/qu_overlay.js"
import { parseQuMessage, encodeSubscribe } from "./video/qu_wire.js"

export type ExecutionEnvironment = {
    main: boolean
    worker: boolean
}

export type InfoEvent = CustomEvent<
    { type: "app", app: App } |
    { type: "serverMessage", message: string } |
    { type: "connectionComplete", capabilities: StreamCapabilities } |
    { type: "videoReady" } |
    { type: "connectionStatus", status: ConnectionStatus } |
    { type: "addDebugLine", line: string, additional?: LogMessageInfo } |
    // M4 cursor P1: host-authority auto mouse mode transitioned (cursor-channel.md §3).
    { type: "cursorAutoMode", locked: boolean }
>
export type InfoEventListener = (event: InfoEvent) => void

export function getStreamerSize(settings: Settings, viewerScreenSize: [number, number]): [number, number] {
    let width, height
    if (settings.videoSize == "720p") {
        width = 1280
        height = 720
    } else if (settings.videoSize == "1080p") {
        width = 1920
        height = 1080
    } else if (settings.videoSize == "1440p") {
        width = 2560
        height = 1440
    } else if (settings.videoSize == "4k") {
        width = 3840
        height = 2160
    } else if (settings.videoSize == "custom") {
        width = settings.videoSizeCustom.width
        height = settings.videoSizeCustom.height
    } else { // native
        width = viewerScreenSize[0]
        height = viewerScreenSize[1]
    }
    return [width, height]
}

function getVideoCodecHint(settings: Settings): VideoCodecSupport {
    let videoCodecHint = emptyVideoCodecs()
    if (settings.videoCodec == "h264") {
        videoCodecHint.H264 = true
        videoCodecHint.H264_HIGH8_444 = true
    } else if (settings.videoCodec == "h265") {
        videoCodecHint.H265 = true
        videoCodecHint.H265_MAIN10 = true
        videoCodecHint.H265_REXT8_444 = true
        videoCodecHint.H265_REXT10_444 = true
    } else if (settings.videoCodec == "av1") {
        videoCodecHint.AV1_MAIN8 = true
        videoCodecHint.AV1_MAIN10 = true
    } else if (settings.videoCodec == "auto") {
        videoCodecHint = allVideoCodecs()
    }

    if (isFirefox()) {
        videoCodecHint.AV1_MAIN10 = false
        videoCodecHint.AV1_HIGH10_444 = false
    }

    return videoCodecHint
}

function isFirefox(): boolean {
    return navigator.userAgent.includes("Firefox/")
}

const WEBRTC_CONNECT_TIMEOUT_MS = 15000
const FALLBACK_RECONNECT_DELAY_MS = 500
// M4 stall watchdog tick period. The ladder escalates at most one rung per
// tick; thresholds live in session_ux.ts DEFAULT_WATCHDOG_CONFIG.
const WATCHDOG_TICK_MS = 250

export class Stream implements Component {
    private logger: Logger = new Logger()

    private api: Api

    private hostId: number
    private appId: number

    private permissions: StreamPermissions
    private settings: Settings

    private divElement = document.createElement("div")
    private eventTarget = new EventTarget()

    private ws: WebSocket
    private iceServers: Array<RTCIceServer> | null = null
    private transportOverride: TransportType | null = null

    private videoRenderer: VideoRenderer | null = null
    private audioPlayer: AudioPlayer | null = null
    // M4 cursor P1: only set when settings.mouseMode == "auto" and the
    // transport is WebRTC — non-auto sessions never allocate this (#5: no
    // behavior change otherwise). Recreated in createVideoRenderer() so it
    // survives transport restarts, matching the FEC/QU wiring pattern.
    private cursorAutoMode: CursorAutoMode | null = null
    // M4 auto-runtime (mid-session mouseMode switch): tracks the physical
    // RTCDataChannel a 'message' listener has been attached to. setOnCursorChannel
    // re-registration is harmless (it just overwrites the transport's stash
    // callback), but a second addEventListener on the SAME channel would
    // double-fire cursorAutoMode.onVisibility — this guards setCursorAutoEnabled()
    // against that when it's called repeatedly at runtime (enable/disable/
    // re-enable) against a still-live transport.
    private cursorChannelWired: RTCDataChannel | null = null

    private input: StreamInput
    private stats: StreamStats

    private streamerSize: [number, number]
    private hasConnectionComplete = false
    private hasVideoReady = false
    private hasDispatchedVideoReady = false

    // ── M4 stall watchdog (field issue #1): pure ladder + thin wiring ──────
    private watchdog = new StallWatchdog()
    private watchdogInterval: number | null = null
    private watchdogBusy = false
    private lastFramesDecoded: number | null = null
    private stallIndicator = document.createElement("div")

    constructor(api: Api, hostId: number, appId: number, settings: Settings, viewerScreenSize: [number, number], permissions: StreamPermissions) {
        this.logger.addInfoListener((info, type) => {
            this.debugLog(info, { type: type ?? undefined })
        })

        this.api = api

        this.hostId = hostId
        this.appId = appId

        this.permissions = permissions
        this.settings = settings

        this.streamerSize = getStreamerSize(settings, viewerScreenSize)

        this.ws = this.createControlWebSocket()
        this.sendInitMessage()

        // Stream Input
        const streamInputConfig = defaultStreamInputConfig()
        Object.assign(streamInputConfig, {
            mouseMode: this.settings.mouseMode,
            mouseScrollMode: this.settings.mouseScrollMode,
            touchMode: this.settings.touchMode,
            localCursorSensitivity: this.settings.localCursorSensitivity,
            controllerConfig: this.settings.controllerConfig
        })
        this.input = new StreamInput(streamInputConfig)

        // Stream Stats. Only non-sensitive, browser-observable settings are
        // included in exported benchmark manifests; host/app query identifiers
        // are deliberately omitted.
        const benchmarkContext: BenchmarkContext = {
            pageUrl: window.location.href,
            userAgent: navigator.userAgent,
            streamSettings: {
                bitrateKbps: settings.bitrate,
                width: this.streamerSize[0],
                height: this.streamerSize[1],
                fps: settings.fps,
                requestedVideoCodec: settings.videoCodec,
                requestedHdr: settings.hdr,
                requestedDataTransport: settings.dataTransport,
                iceTransportPolicy: settings.iceTransportPolicy ?? "all",
                playAudioLocal: settings.playAudioLocal,
                videoFrameQueueSize: settings.videoFrameQueueSize,
                audioSampleQueueSize: settings.audioSampleQueueSize,
            },
        }
        this.stats = new StreamStats(this.logger, benchmarkContext)

        // M4 stall watchdog: user-visible indicator + hidden-tab pause (a
        // hidden tab legitimately stops rendering; it must not escalate).
        this.stallIndicator.classList.add("stream-stall-indicator")
        this.stallIndicator.textContent = "Connection stalled — recovering…"
        this.stallIndicator.hidden = true
        this.divElement.appendChild(this.stallIndicator)
        document.addEventListener("visibilitychange", () => {
            if (document.visibilityState === "visible") {
                this.watchdog.resume(performance.now())
            } else {
                this.watchdog.pause()
            }
        })
    }

    private debugLog(message: string, additional?: LogMessageInfo) {
        for (const line of message.split("\n")) {
            const event: InfoEvent = new CustomEvent("stream-info", {
                detail: { type: "addDebugLine", line, additional }
            })

            this.eventTarget.dispatchEvent(event)
        }
    }
    private resetVideoReadyState() {
        this.hasConnectionComplete = false
        this.hasVideoReady = false
        this.hasDispatchedVideoReady = false
    }
    private markConnectionComplete() {
        this.hasConnectionComplete = true
        this.tryDispatchVideoReady()
    }
    private markVideoReady() {
        this.hasVideoReady = true
        this.tryDispatchVideoReady()
    }
    private tryDispatchVideoReady() {
        if (!this.hasConnectionComplete || !this.hasVideoReady || this.hasDispatchedVideoReady) {
            return
        }

        this.hasDispatchedVideoReady = true
        const event: InfoEvent = new CustomEvent("stream-info", {
            detail: { type: "videoReady" }
        })
        this.eventTarget.dispatchEvent(event)
        this.startWatchdog()
    }

    private async onMessage(message: StreamServerMessage) {
        if ("DebugLog" in message) {
            const debugLog = message.DebugLog

            this.debugLog(debugLog.message, {
                type: debugLog.ty ?? undefined
            })
        } else if ("UpdateApp" in message) {
            const event: InfoEvent = new CustomEvent("stream-info", {
                detail: { type: "app", app: message.UpdateApp.app }
            })

            this.eventTarget.dispatchEvent(event)
        } else if ("ConnectionComplete" in message) {
            const capabilities = message.ConnectionComplete.capabilities
            const formatRaw = message.ConnectionComplete.format
            const width = message.ConnectionComplete.width
            const height = message.ConnectionComplete.height
            const fps = message.ConnectionComplete.fps

            const audioSampleRate = message.ConnectionComplete.audio_sample_rate
            const audioChannelCount = message.ConnectionComplete.audio_channel_count
            const audioStreams = message.ConnectionComplete.audio_streams
            const audioCoupledStreams = message.ConnectionComplete.audio_coupled_streams
            const audioSamplesPerFrame = message.ConnectionComplete.audio_samples_per_frame
            const audioMapping = message.ConnectionComplete.audio_mapping

            const format = getSelectedVideoCodec(formatRaw)
            if (format == null) {
                this.debugLog(`Video Format ${formatRaw} was not found! Couldn't start stream!`, { type: "fatal" })
                return
            }

            const event: InfoEvent = new CustomEvent("stream-info", {
                detail: { type: "connectionComplete", capabilities }
            })

            this.eventTarget.dispatchEvent(event)

            this.input.onStreamStart(capabilities, [width, height])

            this.stats.setVideoInfo(format ?? "Unknown", width, height, fps)
            // HDR state will be set when server sends HdrModeUpdate message
            // Don't initialize from settings.hdr because that's just the user's preference,
            // not the actual HDR state (which depends on host support, display, and codec)
            if (this.settings.hdr) {
                this.debugLog("HDR requested by user, waiting for host confirmation...")
            }

            // we should allow streaming without audio
            if (!this.audioPlayer) {
                showNotification("Failed to find supported audio player -> audio is missing.")
            }

            if (!this.videoRenderer || !this.audioPlayer) {
                throw "Video renderer or audio player not initialized!"
            }

            await Promise.all([
                this.videoRenderer.setup({
                    codec: format,
                    fps,
                    width,
                    height,
                }),
                this.audioPlayer.setup({
                    sampleRate: audioSampleRate,
                    channels: audioChannelCount,
                    streams: audioStreams,
                    coupledStreams: audioCoupledStreams,
                    samplesPerFrame: audioSamplesPerFrame,
                    mapping: audioMapping,
                })
            ])

            this.markConnectionComplete()
        } else if ("ConnectionTerminated" in message) {
            const code = message.ConnectionTerminated.error_code

            this.debugLog(`ConnectionTerminated with code ${code}`, { type: "fatalDescription" })
            this.stopWatchdog()
        }
        // -- WebRTC Config
        else if ("Setup" in message) {
            const iceServers = message.Setup.ice_servers

            this.iceServers = iceServers

            this.debugLog(`window.isSecureContext: ${window.isSecureContext}`)
            this.debugLog(`Using WebRTC Ice Servers: ${createPrettyList(
                iceServers.map(server => server.urls).reduce((list, url) => list.concat(url), [])
            )}`)

            await this.startConnection()
        }
        // -- WebRTC
        else if ("WebRtc" in message) {
            const webrtcMessage = message.WebRtc
            if (this.transport instanceof WebRTCTransport) {
                this.transport.onReceiveMessage(webrtcMessage)
            } else {
                this.debugLog(`Received WebRTC message but transport is currently ${this.transport?.implementationName}`)
            }
        }
    }

    async startConnection() {
        this.debugLog(`Permissions: ${JSON.stringify(this.permissions)}`)

        const desiredTransport = this.transportOverride ?? this.settings.dataTransport
        this.debugLog(`Using transport: ${desiredTransport}`)

        if (desiredTransport == "auto") {
            let shutdownReason = await this.tryWebRTCTransport()

            if (shutdownReason == "failednoconnect") {
                this.debugLog("Failed to establish WebRTC connection. Falling back to Web Socket transport.", { type: "ifErrorDescription" })
                await this.restartWithFreshTransportFallback("websocket")
                return
            }
        } else if (desiredTransport == "webrtc") {
            await this.tryWebRTCTransport()
        } else if (desiredTransport == "websocket") {
            await this.tryWebSocketTransport()
        }

        this.stopWatchdog()
        this.debugLog("Tried all configured transport options but no connection was possible", { type: "fatal" })
    }

    private transport: Transport | null = null

    private createControlWebSocket(): WebSocket {
        const wsApiHost = this.api.host_url.replace(/^http(s)?:/, "ws$1:")
        const ws = new WebSocket(`${wsApiHost}/host/stream`)

        ws.addEventListener("error", (event) => {
            if (this.ws !== ws) {
                return
            }
            this.onError(event)
        })
        ws.addEventListener("open", () => {
            if (this.ws !== ws) {
                return
            }
            this.onWsOpen()
        })
        ws.addEventListener("close", () => {
            if (this.ws !== ws) {
                return
            }
            this.onWsClose()
        })
        ws.addEventListener("message", (event) => {
            if (this.ws !== ws) {
                return
            }
            this.onRawWsMessage(event)
        })

        return ws
    }
    private sendInitMessage() {
        this.sendWsMessage({
            Init: {
                host_id: this.hostId,
                app_id: this.appId,
                video_frame_queue_size: this.settings.videoFrameQueueSize,
                audio_sample_queue_size: this.settings.audioSampleQueueSize,
            }
        })
    }
    private async restartWithFreshTransportFallback(transport: TransportType): Promise<void> {
        this.transportOverride = transport
        await this.restartSessionWithFreshWs()
    }
    /**
     * Full session restart (transport fallback + M4 watchdog `reconnect`
     * rung): tears down the transport and the control socket, then re-inits.
     * The server spawns a fresh streamer for the new control socket.
     */
    private async restartSessionWithFreshWs(): Promise<void> {
        this.stopWatchdog()
        this.resetVideoReadyState()

        if (this.transport) {
            await this.transport.close()
            this.transport = null
        }

        this.wsSendBuffer.length = 0
        const oldWs = this.ws

        if (oldWs.readyState == WebSocket.OPEN || oldWs.readyState == WebSocket.CONNECTING) {
            oldWs.close()
            await new Promise<void>((resolve) => {
                const timeout = window.setTimeout(() => resolve(), 1000)
                oldWs.addEventListener("close", () => {
                    window.clearTimeout(timeout)
                    resolve()
                }, { once: true })
            })
        }

        await new Promise((resolve) => window.setTimeout(resolve, FALLBACK_RECONNECT_DELAY_MS))

        this.ws = this.createControlWebSocket()
        this.sendInitMessage()
    }

    // ── M4 stall watchdog wiring (field issue #1) ──────────────────────────
    // The pure ladder lives in session_ux.ts; this layer only feeds it wall
    // clock / frame signals and executes the returned actions.

    private startWatchdog() {
        this.stopWatchdog()
        this.lastFramesDecoded = null
        this.watchdog.start(performance.now())
        this.watchdogInterval = window.setInterval(() => { void this.watchdogTick() }, WATCHDOG_TICK_MS)
    }
    private stopWatchdog() {
        if (this.watchdogInterval != null) {
            window.clearInterval(this.watchdogInterval)
            this.watchdogInterval = null
        }
        this.watchdog.stop()
        this.stallIndicator.hidden = true
    }
    /** Frame signal from the data-channel pipelines (FEC / websocket data). */
    private watchdogFrameReceived() {
        this.runWatchdogActions(this.watchdog.frameReceived(performance.now()))
    }
    private async watchdogTick(): Promise<void> {
        if (this.watchdogBusy) {
            return
        }
        this.watchdogBusy = true
        try {
            // Videotrack pipelines deliver frames inside the browser media
            // stack (no per-frame callback in this layer): poll the receiver's
            // cumulative framesDecoded counter as the frame signal. Data
            // pipelines signal directly from their receive listeners instead.
            if (this.transport instanceof WebRTCTransport) {
                const decoded = await this.transport.getVideoFramesDecoded()
                if (decoded != null && decoded !== this.lastFramesDecoded) {
                    const first = this.lastFramesDecoded == null
                    this.lastFramesDecoded = decoded
                    if (!first) {
                        this.watchdogFrameReceived()
                    }
                }
            }
        } finally {
            this.watchdogBusy = false
        }
        this.runWatchdogActions(this.watchdog.tick(performance.now()))

        // M4 cursor P1: piggyback on the watchdog's 250ms tick instead of a
        // second interval — commits any pending lock/unlock transition once
        // its hysteresis window elapses (the host only sends POS on change).
        if (this.cursorAutoMode) {
            this.applyCursorAutoAction(this.cursorAutoMode.tick(performance.now()))
        }
    }
    // M4 cursor P1: shared by the initial state seed, cursor channel messages,
    // and the watchdog-piggybacked tick — always keeps StreamInput and the
    // dispatched InfoEvent in lockstep.
    private emitCursorAutoState(locked: boolean) {
        this.input.setAutoLocked(locked)
        const event: InfoEvent = new CustomEvent("stream-info", {
            detail: { type: "cursorAutoMode", locked }
        })
        this.eventTarget.dispatchEvent(event)
    }
    private applyCursorAutoAction(action: CursorAutoAction | null) {
        if (!action) {
            return
        }
        this.emitCursorAutoState(action.type === 'lock')
    }
    // M4 auto-runtime (field report: picking "auto" mid-session did nothing):
    // enable/disable the host-authority auto mouse mode wiring at runtime,
    // not just at pipeline creation. createVideoRenderer() calls this with
    // the startup value; onMouseModeChanged() calls it again on every
    // runtime mouseMode switch.
    private setCursorAutoEnabled(enabled: boolean) {
        if (!enabled) {
            if (!this.cursorAutoMode) {
                // Already disabled (or never enabled) — nothing to reset, and
                // no spurious emit for non-auto sessions (#5: no behavior
                // change otherwise).
                return
            }
            this.cursorAutoMode = null
            // Reset StreamInput/ViewerApp to the non-auto baseline: cursor:none
            // off, autoLocked false. The cursor channel's 'message' listener
            // (if attached — see cursorChannelWired) stays registered; it just
            // becomes a no-op since it routes through this.cursorAutoMode,
            // which is now null.
            this.emitCursorAutoState(false)
            return
        }

        if (!(this.transport instanceof WebRTCTransport)) {
            return
        }

        const cursorTransport = this.transport
        // Re-created on every enable (startup or runtime) so it always starts
        // back in the default unlocked (follow) state instead of carrying
        // over stale lock state from a prior enable/disable cycle.
        const cursorAutoMode = new CursorAutoMode()
        this.cursorAutoMode = cursorAutoMode

        // Seed the UI/input with the initial (unlocked/follow) state: the
        // host only sends POS on change, so nothing else would ever emit
        // this baseline if the host cursor stays visible the whole session.
        this.emitCursorAutoState(cursorAutoMode.locked)

        // setOnCursorChannel's stash callback fires immediately if the
        // channel already arrived (see webrtc.ts) — this is what makes
        // enabling mid-session (after the channel is already live) work.
        // Re-registering here on every enable is harmless: the transport
        // only ever keeps the latest callback.
        cursorTransport.setOnCursorChannel((cursorCh: RTCDataChannel) => {
            if (this.cursorChannelWired === cursorCh) {
                // Already listening on this physical channel from a prior
                // enable — attaching a second 'message' listener would
                // double-fire cursorAutoMode.onVisibility per host sample.
                return
            }
            this.cursorChannelWired = cursorCh

            cursorCh.addEventListener('message', (ev: MessageEvent) => {
                // Read the field (not the `cursorAutoMode` closed over above)
                // so a machine swapped in by a later disable/re-enable cycle
                // takes over immediately without needing its own listener.
                const activeCursorAutoMode = this.cursorAutoMode
                if (!activeCursorAutoMode) {
                    return
                }
                const buf: ArrayBuffer = ev.data instanceof ArrayBuffer
                    ? ev.data : ev.data.buffer
                const msg = parseCursorMessage(buf)
                if (msg) {
                    this.applyCursorAutoAction(activeCursorAutoMode.onVisibility(msg.visible, performance.now()))
                }
            })
        })
    }
    // M4 auto-runtime: call when mouseMode changes at runtime (e.g. the
    // sidebar mouse-mode selector) so the cursor-channel wiring engages or
    // disengages immediately instead of only at the next pipeline creation
    // (field report: picking "auto" mid-session used to do nothing because
    // the wiring was gated on settings.mouseMode read once in
    // createVideoRenderer). Persists the choice onto settings so a later
    // transport restart/reconnect also honors it.
    onMouseModeChanged(mode: MouseMode) {
        this.settings.mouseMode = mode
        this.setCursorAutoEnabled(mode === "auto")
    }
    private runWatchdogActions(actions: WatchdogAction[]) {
        for (const action of actions) {
            switch (action.type) {
                case "stall":
                    this.debugLog("Stall watchdog: no frames delivered — showing indicator", { type: "informError" })
                    this.stallIndicator.hidden = false
                    break
                case "recovered":
                    this.debugLog(`Stall watchdog: recovered after ${Math.round(action.stalledMs)}ms`, { type: "recover" })
                    this.stallIndicator.hidden = true
                    break
                case "requestIdr":
                    // Over the signaling socket — survives a dead data path.
                    this.debugLog(`Stall watchdog: requesting IDR (attempt ${action.attempt})`)
                    this.sendWsMessage("RequestIdr")
                    break
                case "restartIce":
                    // WebRTC only: the websocket transport has no ICE — the
                    // ladder proceeds to the reconnect rung on its own.
                    if (this.transport instanceof WebRTCTransport) {
                        this.debugLog("Stall watchdog: requesting ICE restart")
                        this.sendWsMessage("RestartIce")
                    }
                    break
                case "reconnect":
                    this.debugLog("Stall watchdog: escalating to full reconnect", { type: "ifErrorDescription" })
                    void this.restartSessionWithFreshWs()
                    break
            }
        }
    }

    private setTransport(transport: Transport) {
        if (this.transport) {
            const oldTransport = this.transport
            // Detach signaling before closing: a still-gathering old WebRTC
            // peer must not route stale ICE candidates (with the previous
            // ufrag) into the WebSocket after the swap (2026-07-14 ICE
            // reconnect investigation - candidate-misrouting amplifier).
            if (oldTransport instanceof WebRTCTransport) {
                oldTransport.onsendmessage = null
            }
            oldTransport.close()
        }

        this.transport = transport

        this.input.setTransport(this.transport)
        this.stats.setTransport(this.transport)

        const rtt = this.transport.getChannel(TransportChannelId.RTT)
        if (rtt.type == "data") {
            rtt.addReceiveListener((data) => {
                const buffer = new ByteBuffer(data.byteLength)
                buffer.putU8Array(new Uint8Array(data))
                buffer.flip()

                const ty = buffer.getU8()
                if (ty == 0) {
                    rtt.send(data)
                }
            })
        } else {
            this.debugLog("Failed to get rtt as data transport channel. Cannot respond to rtt packets")
        }

        // Setup GENERAL channel listener for HDR mode updates
        const generalChannel = this.transport.getChannel(TransportChannelId.GENERAL)
        this.debugLog(`[GENERAL] Setting up GENERAL channel listener, type=${generalChannel.type}`)
        if (generalChannel.type === "data") {
            generalChannel.addReceiveListener((data: ArrayBuffer) => {
                this.onGeneralChannelMessage(data)
            })
            this.debugLog(`[GENERAL] GENERAL channel listener registered`)
        } else {
            this.debugLog(`[GENERAL] Cannot register listener, channel type is not 'data'`)
        }
    }

    private onGeneralChannelMessage(data: ArrayBuffer) {
        this.debugLog(`[GENERAL] Received message on GENERAL channel, size=${data.byteLength}`)
        const buffer = new Uint8Array(data)
        if (buffer.length < 2) {
            this.debugLog(`[GENERAL] Message too short: ${buffer.length} bytes`)
            return
        }

        const textLength = (buffer[0] << 8) | buffer[1]
        if (buffer.length < 2 + textLength) {
            this.debugLog(`[GENERAL] Message incomplete: expected ${2 + textLength} bytes, got ${buffer.length}`)
            return
        }

        const text = new TextDecoder().decode(buffer.slice(2, 2 + textLength))
        this.debugLog(`[GENERAL] Parsed message: ${text}`)
        try {
            const message: GeneralServerMessage = JSON.parse(text)
            this.handleGeneralMessage(message)
        } catch (err) {
            this.debugLog(`Failed to parse general message: ${err}`)
        }
    }

    private handleGeneralMessage(message: GeneralServerMessage) {
        if ("HdrModeUpdate" in message) {
            const hdrUpdate = message.HdrModeUpdate
            if (hdrUpdate) {
                const enabled = hdrUpdate.enabled
                this.debugLog(`HDR mode ${enabled ? "enabled" : "disabled"}`)
                this.setHdrMode(enabled)
            }
        } else if ("ConnectionStatusUpdate" in message) {
            const statusUpdate = message.ConnectionStatusUpdate
            if (statusUpdate) {
                const status = statusUpdate.status
                const event: InfoEvent = new CustomEvent("stream-info", {
                    detail: { type: "connectionStatus", status }
                })
                this.eventTarget.dispatchEvent(event)
            }
        }
    }

    private setHdrMode(enabled: boolean) {
        this.stats.setHdrEnabled(enabled)
        if (this.videoRenderer) {
            if ("setHdrMode" in this.videoRenderer && typeof this.videoRenderer.setHdrMode === "function") {
                this.videoRenderer.setHdrMode(enabled)
            }
        }
    }

    private sendGeneralMessage(message: GeneralClientMessage): boolean {
        const general = this.transport?.getChannel(TransportChannelId.GENERAL)

        if (!general || general.type != "data") {
            return false
        }

        const text = JSON.stringify(message)

        const buffer = BIG_BUFFER
        buffer.reset()
        buffer.putU16(text.length)
        buffer.putUtf8Raw(text)
        buffer.flip()

        general.send(buffer.getRemainingBuffer().buffer)

        return true
    }

    private async tryWebRTCTransport(): Promise<TransportShutdown> {
        if (!this.permissions.allow_transport_webrtc) {
            this.debugLog("Not trying WebRTC transport because permissions disallow it")
            return "failednoconnect"
        }

        this.debugLog("Trying WebRTC transport")

        this.sendWsMessage({
            SetTransport: "WebRTC"
        })

        if (!this.iceServers) {
            this.debugLog(`Failed to try WebRTC Transport: no ice servers available`)
            return "failednoconnect"
        }

        const transport = new WebRTCTransport(this.logger)
        transport.onsendmessage = (message) => this.sendWsMessage({ WebRtc: message })

        transport.initPeer({
            iceServers: this.iceServers,
            // Feature #2: relay-only forces all media/candidates through TURN
            // (443/TLS) so a locked network never needs a direct/UDP path.
            iceTransportPolicy: this.settings.iceTransportPolicy ?? "all"
        })
        this.setTransport(transport)

        const videoCodecSupport = await this.createPipelines()
        if (!videoCodecSupport) {
            this.debugLog("No video pipeline was found for the codec that was specified. If you're unsure which codecs are supported use H264.", { type: "fatalDescription" })

            await transport.close()
            return "failednoconnect"
        }

        // Starting the stream will start negotiation
        await this.startStream(videoCodecSupport)

        // Wait for negotiation, but don't let a stuck ICE check block fallback forever.
        const result = await new Promise<boolean>((resolve) => {
            const timeout = window.setTimeout(async () => {
                this.debugLog(`WebRTC negotiation timed out after ${WEBRTC_CONNECT_TIMEOUT_MS}ms`)
                transport.onconnect = null
                transport.onclose = null
                await transport.close()
                resolve(false)
            }, WEBRTC_CONNECT_TIMEOUT_MS)

            transport.onconnect = () => {
                window.clearTimeout(timeout)
                resolve(true)
            }
            transport.onclose = () => {
                window.clearTimeout(timeout)
                resolve(false)
            }
        })
        this.debugLog(`WebRTC negotiation success: ${result}`)

        if (!result) {
            return "failednoconnect"
        }

        return new Promise((resolve) => {
            transport.onclose = (shutdown) => {
                resolve(shutdown)
            }
        })
    }
    private async tryWebSocketTransport() {
        if (!this.permissions.allow_transport_websockets) {
            this.debugLog("Not trying WebSocket transport becaues permissions disallow it")
            return
        }

        this.debugLog("Trying Web Socket transport")

        this.sendWsMessage({
            SetTransport: "WebSocket"
        })

        const transport = new WebSocketTransport(this.ws, BIG_BUFFER, this.logger)

        this.setTransport(transport)

        const videoCodecSupport = await this.createPipelines()
        if (!videoCodecSupport) {
            this.debugLog("Failed to start stream because no video pipeline with support for the specified codec was found!", { type: "fatalDescription" })
            return
        }

        await this.startStream(videoCodecSupport)

        return new Promise((resolve) => {
            transport.onclose = (shutdown) => {
                resolve(shutdown)
            }
        })
    }

    private async createPipelines(): Promise<VideoCodecSupport | null> {
        // Print supported pipes
        const pipesInfo = await gatherPipeInfo()

        this.logger.debug(`Supported Pipes: {`)
        let isFirst = true
        for (const [pipe, info] of pipesInfo) {
            this.logger.debug(`${isFirst ? "" : ","}"${pipe.name}": ${JSON.stringify(info)}`)
            isFirst = false
        }
        this.logger.debug(`}`)

        // Create pipelines
        const [supportedVideoCodecs] = await Promise.all([this.createVideoRenderer(), this.createAudioPlayer()])

        const videoPipelineName = `${this.transport?.getChannel(TransportChannelId.HOST_VIDEO).type} (transport) -> ${this.videoRenderer?.implementationName} (renderer)`
        this.debugLog(`Using video pipeline: ${videoPipelineName}`)

        const audioPipelineName = `${this.transport?.getChannel(TransportChannelId.HOST_AUDIO).type} (transport) -> ${this.audioPlayer?.implementationName} (player)`
        this.debugLog(`Using audio pipeline: ${audioPipelineName}`)

        this.stats.setVideoPipeline(videoPipelineName, this.videoRenderer)
        this.stats.setAudioPipeline(audioPipelineName, this.audioPlayer)

        return supportedVideoCodecs
    }
    private async createVideoRenderer(): Promise<VideoCodecSupport | null> {
        if (this.videoRenderer) {
            this.debugLog("Found an old video renderer -> cleaning it up")

            this.videoRenderer.unmount(this.divElement)
            this.videoRenderer.cleanup()
            this.videoRenderer = null
        }
        if (!this.transport) {
            this.debugLog("Failed to setup video without transport")
            return null
        }

        const codecHint = getVideoCodecHint(this.settings)
        this.debugLog(`Codec Hint by the user: ${JSON.stringify(codecHint)}`)

        if (!hasAnyCodec(codecHint)) {
            this.debugLog("Couldn't find any supported video format. Change the codec option to H264 in the settings if you're unsure which codecs are supported.", { type: "fatalDescription" })
            return null
        }

        const transportCodecSupport = await this.transport.setupHostVideo({
            type: ["videotrack", "data"]
        })
        this.debugLog(`Transport supports these video codecs: ${JSON.stringify(transportCodecSupport)}`)

        const videoSettings: VideoPipelineOptions = {
            supportedVideoCodecs: andVideoCodecs(codecHint, transportCodecSupport),
            canvasRenderer: this.settings.canvasRenderer,
            forceVideoElementRenderer: this.settings.forceVideoElementRenderer,
            canvasVsync: this.settings.canvasVsync
        }

        let pipelineCodecSupport
        const video = this.transport.getChannel(TransportChannelId.HOST_VIDEO)
        if (video.type == "videotrack") {
            // ── U2 P1: FEC pipeline over video_fec DataChannel ──────────────
            // When enableVideoFec is true and we have a WebRTC transport with
            // FEC channels ready, use FecDecodePipe as the head. The host RTP
            // videotrack keeps flowing but is not attached to any renderer in
            // this mode (P1 test path). The check must live inside the
            // "videotrack" branch because WebRTC always yields type="videotrack"
            // for HOST_VIDEO — the old "data" branch was dead code.
            if (this.settings.enableVideoFec && this.transport instanceof WebRTCTransport) {
                const fecChannels = this.transport.getFecChannels()
                if (fecChannels) {
                    const { data: fecData, ack: fecAck } = fecChannels

                    // Build FEC pipeline (FecDecodePipe head + remainder of data chain)
                    const { videoRenderer, supportedCodecs, error } = await buildFecVideoPipeline(videoSettings, this.logger)
                    if (error) return null
                    pipelineCodecSupport = supportedCodecs

                    // Pull the FecDecodePipe instance from the renderer chain so we
                    // can configure its onAck callback.
                    let fecPipe: FecDecodePipe | null = null
                    let cursor: import("./pipeline/index.js").Pipe | null = videoRenderer
                    while (cursor) {
                        if (cursor instanceof FecDecodePipe) { fecPipe = cursor; break }
                        cursor = cursor.getBase()
                    }

                    if (fecPipe) {
                        // Wire ack: send encodeAck on the ack channel
                        fecPipe.setOnAck((highest: number) => {
                            fecAck.send(encodeAck(highest))
                        })
                    }

                    videoRenderer.mount(this.divElement)

                    // Send SUBSCRIBE to activate the host FEC sender.
                    // Guard: check readyState in addition to the open event so
                    // that re-setup calls (e.g. codec renegotiation) also send
                    // SUBSCRIBE when the channel is already open.
                    fecAck.addEventListener("open", () => {
                        fecAck.send(SUBSCRIBE_MESSAGE)
                    })
                    if (fecAck.readyState === "open") {
                        fecAck.send(SUBSCRIBE_MESSAGE)
                    }

                    // Receive FEC symbols from video_fec DataChannel
                    fecData.addEventListener("message", (event: MessageEvent) => {
                        const buf: ArrayBuffer = event.data instanceof ArrayBuffer
                            ? event.data
                            : event.data.buffer
                        this.markVideoReady()
                        this.watchdogFrameReceived()
                        videoRenderer.submitPacket(buf)

                        // After each symbol, check if IDR is needed
                        if (videoRenderer.pollRequestIdr()) {
                            fecAck.send(NEEDS_IDR_MESSAGE)
                        }
                    })

                    this.videoRenderer = videoRenderer
                } else {
                    // FEC channels not yet received — fall back to the normal
                    // videotrack pipeline so the stream still plays.
                    this.debugLog("enableVideoFec=true but video_fec channels not yet received; falling back to videotrack pipeline", { type: "ifErrorDescription" })
                    const { videoRenderer, supportedCodecs, error } = await buildVideoPipeline("videotrack", videoSettings, this.logger)
                    if (error) return null
                    pipelineCodecSupport = supportedCodecs
                    videoRenderer.mount(this.divElement)
                    video.addTrackListener((track) => {
                        this.markVideoReady()
                        videoRenderer.setTrack(track)
                    })
                    this.videoRenderer = videoRenderer
                }
            } else {
                // Normal WebRTC videotrack pipeline (FEC disabled or non-WebRTC transport).
                const { videoRenderer, supportedCodecs, error } = await buildVideoPipeline("videotrack", videoSettings, this.logger)

                if (error) {
                    return null
                }
                pipelineCodecSupport = supportedCodecs

                videoRenderer.mount(this.divElement)

                video.addTrackListener((track) => {
                    this.markVideoReady()
                    videoRenderer.setTrack(track)
                })

                this.videoRenderer = videoRenderer
            }
        } else if (video.type == "data") {
            const { videoRenderer, supportedCodecs, error } = await buildVideoPipeline("data", videoSettings, this.logger)

            if (error) {
                return null
            }
            pipelineCodecSupport = supportedCodecs

            videoRenderer.mount(this.divElement)

            video.addReceiveListener((data) => {
                this.markVideoReady()
                this.watchdogFrameReceived()
                videoRenderer.submitPacket(data)

                // data pipeline support requesting idrs over video channel
                if (videoRenderer.pollRequestIdr()) {
                    const buffer = new ByteBuffer(1)

                    buffer.putU8(0)

                    buffer.flip()

                    video.send(buffer.getRemainingBuffer().buffer)
                }
            })

            this.videoRenderer = videoRenderer
        } else {
            this.debugLog(`Failed to create video pipeline with transport channel of type ${video.type} (${this.transport.implementationName})`)
            return null
        }

        // ── U4 P1: QU lossless overlay ────────────────────────────────────────
        // When enableVideoQu is true and a WebRTC transport is active, register a
        // callback that attaches a QuOverlayDom over the renderer element as soon
        // as the video_qu DataChannel arrives (reliable + ordered, bidirectional).
        // Flag-off (default) = zero new work on the hot path.
        if (this.settings.enableVideoQu && this.transport instanceof WebRTCTransport) {
            const quTransport = this.transport
            // The renderer element is the first child of divElement after mount().
            const rendererEl = this.divElement.firstElementChild as HTMLElement | null
            if (rendererEl) {
                const overlay = new QuOverlayDom(
                    rendererEl,
                    this.divElement,
                    this.streamerSize[0],
                    this.streamerSize[1],
                )
                quTransport.setOnQuChannel((quCh: RTCDataChannel) => {
                    // Send QU_SUBSCRIBE to activate the host QU sender (dormant by default).
                    const sub = encodeSubscribe()
                    if (quCh.readyState === 'open') {
                        quCh.send(sub)
                    }
                    quCh.addEventListener('open', () => quCh.send(encodeSubscribe()))
                    // Feed all incoming ArrayBuffers through the overlay state machine.
                    quCh.addEventListener('message', (ev: MessageEvent) => {
                        const buf: ArrayBuffer = ev.data instanceof ArrayBuffer
                            ? ev.data : ev.data.buffer
                        const msg = parseQuMessage(buf)
                        if (msg) overlay.apply(msg)
                    })
                })
            } else {
                this.debugLog('enableVideoQu=true but renderer element not found in divElement; overlay skipped', { type: 'ifErrorDescription' })
            }
        }
        // M4 auto-runtime: startup gate — mid-session mouseMode switches go
        // through onMouseModeChanged() -> setCursorAutoEnabled() instead, so
        // picking "auto" mid-session (previously a no-op) takes effect
        // immediately rather than only on the next createVideoRenderer() call.
        this.setCursorAutoEnabled(this.settings.mouseMode === "auto")

        return pipelineCodecSupport
    }
    private async createAudioPlayer(): Promise<boolean> {
        if (this.audioPlayer) {
            this.debugLog("Found an old audio player -> cleaning it up")

            this.audioPlayer.unmount(this.divElement)
            this.audioPlayer.cleanup()
            this.audioPlayer = null
        }
        if (!this.transport) {
            this.debugLog("Failed to setup audio without transport")
            return false
        }

        this.transport.setupHostAudio({
            type: ["audiotrack", "data"]
        })

        const audio = this.transport?.getChannel(TransportChannelId.HOST_AUDIO)
        if (audio.type == "audiotrack") {
            const { audioPlayer, error } = await buildAudioPipeline("audiotrack", this.settings, this.logger)

            if (error) {
                return false
            }

            audioPlayer.mount(this.divElement)

            audio.addTrackListener((track) => audioPlayer.setTrack(track))

            this.audioPlayer = audioPlayer
        } else if (audio.type == "data") {
            const { audioPlayer, error } = await buildAudioPipeline("data", this.settings, this.logger)

            if (error) {
                return false
            }

            audioPlayer.mount(this.divElement)

            audio.addReceiveListener((data) => {
                audioPlayer.submitPacket(data)
            })

            this.audioPlayer = audioPlayer
        } else {
            this.debugLog(`Cannot find audio pipeline for transport type "${audio.type}"`)
            return false
        }

        return true
    }
    private async startStream(videoCodecSupport: VideoCodecSupport): Promise<void> {
        const settings: StreamSettings = {
            bitrate_kbps: this.settings.bitrate,
            fps: this.settings.fps,
            width: this.streamerSize[0],
            height: this.streamerSize[1],
            play_audio_local: this.settings.playAudioLocal,
            supported_codecs: createSupportedVideoFormatsBits(videoCodecSupport),
            hdr: this.settings.hdr ?? false,
        }

        const message: StreamClientMessage = {
            StartStream: {
                settings
            }
        }
        this.debugLog(`Starting stream with info: ${JSON.stringify(message)}`)
        this.debugLog(`Stream video codec info: ${JSON.stringify(videoCodecSupport)}`)

        // Log HDR requirements if HDR is requested
        if (this.settings.hdr) {
            const hasHdrCodec = videoCodecSupport.H265_MAIN10 || videoCodecSupport.AV1_MAIN10
            if (!hasHdrCodec) {
                this.debugLog(`Warning: HDR requested but no 10-bit codec available. HDR requires H265_MAIN10 or AV1_MAIN10 support.`)
            } else {
                this.debugLog(`HDR codec available: H265_MAIN10=${videoCodecSupport.H265_MAIN10}, AV1_MAIN10=${videoCodecSupport.AV1_MAIN10}`)
            }
        }

        this.sendWsMessage(message)
    }

    mount(parent: HTMLElement): void {
        parent.appendChild(this.divElement)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.divElement)
    }

    getVideoRenderer(): VideoRenderer | null {
        return this.videoRenderer
    }
    getAudioPlayer(): AudioPlayer | null {
        return this.audioPlayer
    }

    // -- Raw Web Socket stuff
    private wsSendBuffer: Array<string> = []

    private onWsOpen() {
        this.debugLog(`Web Socket Open`)

        for (const raw of this.wsSendBuffer.splice(0)) {
            this.ws.send(raw)
        }
    }
    private onWsClose() {
        this.debugLog(`Web Socket Closed`)
    }
    private onError(event: Event) {
        this.debugLog(`Web Socket or WebRtcPeer Error`)

        console.error(`Web Socket or WebRtcPeer Error`, event)
    }

    private sendWsMessage(message: StreamClientMessage) {
        const raw = JSON.stringify(message)
        if (this.ws.readyState == WebSocket.OPEN) {
            this.ws.send(raw)
        } else {
            this.wsSendBuffer.push(raw)
        }
    }
    private onRawWsMessage(event: MessageEvent) {
        const message = event.data
        if (typeof message == "string") {
            const json = JSON.parse(message)

            this.onMessage(json)
        }
    }

    stop(): Promise<boolean> {
        if (!this.sendGeneralMessage("Stop")) {
            return Promise.resolve(false)
        }

        // Wait for the message to get sent
        return new Promise((resolve, _reject) => {
            setTimeout(() => resolve(true), 100)
        })
    }

    // -- Class Api
    addInfoListener(listener: InfoEventListener) {
        this.eventTarget.addEventListener("stream-info", listener as EventListenerOrEventListenerObject)
    }
    removeInfoListener(listener: InfoEventListener) {
        this.eventTarget.removeEventListener("stream-info", listener as EventListenerOrEventListenerObject)
    }

    getInput(): StreamInput {
        return this.input
    }
    getStats(): StreamStats {
        return this.stats
    }

    getStreamerSize(): [number, number] {
        return this.streamerSize
    }
}

function createPrettyList(list: Array<string>): string {
    return `[${list.join(", ")}]`
}
