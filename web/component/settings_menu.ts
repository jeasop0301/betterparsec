import { ControllerConfig } from "../stream/gamepad.js";
import { MouseMode, MouseScrollMode, TouchMode } from "../stream/input.js";
import { PageStyle } from "../styles/index.js";
import { getLanguageOptions, getTranslations, Language, normalizeLanguage } from "../i18n.js";
import { Component, ComponentEvent } from "./index.js";
import { InputComponent, SelectComponent } from "./input.js";
import { SidebarEdge } from "./sidebar/index.js";

export type Settings = {
    sidebarEdge: SidebarEdge,
    bitrate: number
    videoFrameQueueSize: number
    videoSize: "720p" | "1080p" | "1440p" | "4k" | "native" | "custom"
    videoSizeCustom: {
        width: number
        height: number
    },
    fps: number
    videoCodec: StreamCodec,
    forceVideoElementRenderer: boolean
    canvasRenderer: boolean
    canvasVsync: boolean
    playAudioLocal: boolean
    audioSampleQueueSize: number
    mouseScrollMode: MouseScrollMode
    mouseMode: MouseMode
    touchMode: TouchMode
    localCursorSensitivity: number
    controllerConfig: ControllerConfig
    dataTransport: TransportType
    iceTransportPolicy?: IceTransportPolicy
    language: Language
    enterFullscreenOnStreamStart: boolean
    toggleFullscreenWithKeybind: boolean
    pageStyle: PageStyle
    hdr: boolean
    useSelectElementPolyfill: boolean
    // U2 P1: enable Tetrys FEC pipeline over video_fec DataChannel.
    // Default false (RTP videotrack path unchanged). Set true for test mode.
    enableVideoFec: boolean
    // U4 P1: enable QU lossless overlay over video_qu DataChannel.
    // Default false (zero hot-path cost). Set true to receive QU_TILE overlays.
    enableVideoQu: boolean
    // Clipboard sync v1 (text-only, research/05 gap vs Parsec/DCV): default
    // true — browser clipboard permission prompts provide the actual
    // consent gate (Parsec ships this default-on too).
    clipboardSync: boolean
    // M4 cursor P2 (cursor-channel.md §P2): render the host cursor shape
    // client-side via CSS instead of relying on the video-baked cursor.
    // Default false — Sunshine still bakes the cursor into the video until
    // the display_cursor fork config lands, so with this off behavior is
    // identical to before P2.
    clientCursor: boolean
}

export type StreamCodec = "h264" | "auto" | "h265" | "av1"
export type TransportType = "auto" | "webrtc" | "websocket"
// "relay" forces all WebRTC traffic through a TURN relay (feature #2: connect
// without WARP over TURN-over-TLS 443 on locked networks). "all" = base default.
export type IceTransportPolicy = "all" | "relay"

import DEFAULT_SETTINGS from "../default_settings.js"
import { StreamPermissions } from "../api_bindings.js";

/// You should use the role default settings instead!
export function globalDefaultSettings(): Settings {
    // We are deep cloning this
    return deepClone(DEFAULT_SETTINGS)
}

function deepClone<T>(value: T): T {
    if (typeof structuredClone == "function") {
        return structuredClone(value)
    } else {
        return JSON.parse(JSON.stringify(value))
    }
}
function deepMerge(target: any, source: any) {
    for (const key in source) {
        const sourceVal = source[key]
        const targetVal = target[key]

        if (
            sourceVal &&
            typeof sourceVal === "object" &&
            !Array.isArray(sourceVal)
        ) {
            target[key] = deepMerge(
                targetVal && typeof targetVal === "object" ? targetVal : {},
                sourceVal
            )
        } else if (sourceVal !== undefined) {
            target[key] = sourceVal
        }
    }
    return target
}

export function getLocalStreamSettings(defaultSettings: Settings) {
    // Start with FULL global defaults
    let settings = globalDefaultSettings()

    // Fill/override with role defaults (even if partial)
    settings = deepMerge(settings, defaultSettings)

    try {
        const json = localStorage.getItem("mlSettings")
        if (json) {
            const loaded = JSON.parse(json)

            // Finally override with user settings
            settings = deepMerge(settings, loaded)
        }
    } catch (e) {
        localStorage.removeItem("mlSettings")
    }

    // Migration
    if (settings?.pageStyle === "old") {
        settings.pageStyle = "moonlight"
    }

    return settings
}
export function setLocalStreamSettings(settings?: Settings) {
    localStorage.setItem("mlSettings", JSON.stringify(settings))
}

export type StreamSettingsChangeListener = (event: ComponentEvent<StreamSettingsComponent>) => void

function makeSettingsValid(permissions: StreamPermissions, settings: Settings) {
    if (permissions.maximum_bitrate_kbps != null && permissions.maximum_bitrate_kbps < settings.bitrate) {
        settings.bitrate = permissions.maximum_bitrate_kbps
    }

    if (!permissions.allow_codec_av1 && settings.videoCodec == "av1") {
        settings.videoCodec = "h265"
    }
    if (!permissions.allow_codec_h265 && settings.videoCodec == "h265") {
        settings.videoCodec = "h264"
    }
    if (!permissions.allow_codec_h264 && settings.videoCodec == "h264") {
        settings.videoCodec = "auto"
    }

    if (!permissions.allow_hdr && settings.hdr) {
        settings.hdr = false
    }

    if (!permissions.allow_transport_webrtc && settings.dataTransport == "webrtc") {
        settings.dataTransport = "auto"
    }
    if (!permissions.allow_transport_websockets && settings.dataTransport == "websocket") {
        settings.dataTransport = "auto"
    }

    if (!Number.isFinite(settings.localCursorSensitivity) || settings.localCursorSensitivity <= 0) {
        settings.localCursorSensitivity = globalDefaultSettings().localCursorSensitivity
    }
}

export class StreamSettingsComponent implements Component {

    private permissions: StreamPermissions

    private divElement: HTMLDivElement = document.createElement("div")

    private sidebarHeader: HTMLHeadingElement = document.createElement("h3")
    private sidebarEdge: SelectComponent

    private streamHeader: HTMLHeadingElement = document.createElement("h3")
    private bitrate: InputComponent
    private fps: InputComponent
    private videoCodec: SelectComponent
    private forceVideoElementRenderer: InputComponent
    private canvasRenderer: InputComponent
    private canvasVsync: InputComponent
    private hdr: InputComponent

    private videoSize: SelectComponent
    private videoSizeWidth: InputComponent
    private videoSizeHeight: InputComponent

    private videoSampleQueueSize: InputComponent

    private audioHeader: HTMLHeadingElement = document.createElement("h3")
    private playAudioLocal: InputComponent
    private audioSampleQueueSize: InputComponent

    private mouseHeader: HTMLHeadingElement = document.createElement("h3")
    private mouseScrollMode: SelectComponent
    private mouseMode: SelectComponent
    private touchMode: SelectComponent
    private localCursorSensitivity: InputComponent

    private controllerHeader: HTMLHeadingElement = document.createElement("h3")
    private controllerInvertAB: InputComponent
    private controllerInvertXY: InputComponent
    private controllerSendIntervalOverride: InputComponent

    private otherHeader: HTMLHeadingElement = document.createElement("h3")
    private dataTransport: SelectComponent
    private language: SelectComponent
    private enterFullscreenOnStreamStart: InputComponent
    private toggleFullscreenWithKeybind: InputComponent

    private pageStyle: SelectComponent

    private useSelectElementPolyfill: InputComponent
    private clipboardSync: InputComponent
    private clientCursor: InputComponent

    private newSection(header: HTMLHeadingElement, title: string): HTMLDivElement {
        header.innerText = title
        const section = document.createElement("div")
        section.classList.add("settings-section")
        section.appendChild(header)
        this.divElement.appendChild(section)
        return section
    }

    constructor(permissions: StreamPermissions, settings: Settings) {
        // Sometimes the normal settings object doesn't have some values, because they change between versions.
        // Use those as fallback
        const defaultSettings_ = globalDefaultSettings()

        makeSettingsValid(permissions, defaultSettings_)
        makeSettingsValid(permissions, settings)

        this.permissions = permissions
        const language = normalizeLanguage(settings?.language ?? defaultSettings_.language)
        const translations = getTranslations(language)
        const i = translations.settings
        const streamI = translations.stream

        // Root div
        this.divElement.classList.add("settings")

        // Sidebar
        const sidebarSection = this.newSection(this.sidebarHeader, i.sidebar)

        this.sidebarEdge = new SelectComponent("sidebarEdge", [
            { value: "left", name: i.left },
            { value: "right", name: i.right },
            { value: "up", name: i.up },
            { value: "down", name: i.down },
        ], {
            displayName: i.sidebarEdge,
            preSelectedOption: settings?.sidebarEdge ?? defaultSettings_.sidebarEdge,
        })
        this.sidebarEdge.addChangeListener(this.onSettingsChange.bind(this))
        this.sidebarEdge.mount(sidebarSection)

        // Video
        const streamSection = this.newSection(this.streamHeader, i.video)

        // Bitrate
        this.bitrate = new InputComponent("bitrate", "number", i.bitrate, {
            defaultValue: defaultSettings_.bitrate.toString(),
            value: settings?.bitrate?.toString(),
            step: "100",
            numberSlider: {
                range_min: Math.min(this.permissions.maximum_bitrate_kbps ?? 1000, 1000),
                range_max: this.permissions.maximum_bitrate_kbps ?? 10000,
            }
        })
        this.bitrate.addChangeListener(this.onSettingsChange.bind(this))
        this.bitrate.mount(streamSection)

        // Fps
        this.fps = new InputComponent("fps", "number", i.fps, {
            defaultValue: defaultSettings_.fps.toString(),
            value: settings?.fps?.toString(),
            step: "100"
        })
        this.fps.addChangeListener(this.onSettingsChange.bind(this))
        this.fps.mount(streamSection)

        // Video Size
        this.videoSize = new SelectComponent("videoSize",
            [
                { value: "720p", name: "720p" },
                { value: "1080p", name: "1080p" },
                { value: "1440p", name: "1440p" },
                { value: "4k", name: "4k" },
                { value: "native", name: i.native },
                { value: "custom", name: i.custom }
            ],
            {
                displayName: i.videoSize,
                preSelectedOption: settings?.videoSize || defaultSettings_.videoSize
            }
        )
        this.videoSize.addChangeListener(this.onSettingsChange.bind(this))
        this.videoSize.mount(streamSection)

        this.videoSizeWidth = new InputComponent("videoSizeWidth", "number", i.videoWidth, {
            defaultValue: defaultSettings_.videoSizeCustom.width.toString(),
            value: settings?.videoSizeCustom?.width.toString()
        })
        this.videoSizeWidth.addChangeListener(this.onSettingsChange.bind(this))
        this.videoSizeWidth.mount(streamSection)

        this.videoSizeHeight = new InputComponent("videoSizeHeight", "number", i.videoHeight, {
            defaultValue: defaultSettings_.videoSizeCustom.height.toString(),
            value: settings?.videoSizeCustom?.height.toString()
        })
        this.videoSizeHeight.addChangeListener(this.onSettingsChange.bind(this))
        this.videoSizeHeight.mount(streamSection)

        // Video Sample Queue Size
        this.videoSampleQueueSize = new InputComponent("videoFrameQueueSize", "number", i.videoFrameQueueSize, {
            defaultValue: defaultSettings_.videoFrameQueueSize.toString(),
            value: settings?.videoFrameQueueSize?.toString()
        })
        this.videoSampleQueueSize.addChangeListener(this.onSettingsChange.bind(this))
        this.videoSampleQueueSize.mount(streamSection)

        // Codec
        const allowedVideoCodecs = [
            { value: "auto", name: i.autoExperimental },
        ]
        if (this.permissions.allow_codec_h264) {
            allowedVideoCodecs.push(
                { value: "h264", name: "H264" },
            )
        }
        if (this.permissions.allow_codec_h265) {
            allowedVideoCodecs.push(
                { value: "h265", name: "H265" },
            )
        }
        if (this.permissions.allow_codec_av1) {
            allowedVideoCodecs.push(
                { value: "av1", name: i.av1Experimental }
            )
        }

        this.videoCodec = new SelectComponent("videoCodec", allowedVideoCodecs, {
            displayName: i.videoCodec,
            preSelectedOption: settings?.videoCodec ?? defaultSettings_.videoCodec
        })
        this.videoCodec.addChangeListener(this.onSettingsChange.bind(this))
        this.videoCodec.mount(streamSection)

        // Force Video Element renderer
        this.forceVideoElementRenderer = new InputComponent("forceVideoElementRenderer", "checkbox", i.forceVideoElementRenderer, {
            checked: settings?.forceVideoElementRenderer ?? defaultSettings_.forceVideoElementRenderer
        })
        this.forceVideoElementRenderer.addChangeListener(this.onSettingsChange.bind(this))
        this.forceVideoElementRenderer.mount(streamSection)

        // Use Canvas Renderer
        this.canvasRenderer = new InputComponent("canvasRenderer", "checkbox", i.useCanvasRenderer, {
            defaultValue: defaultSettings_.canvasRenderer.toString(),
            checked: settings === null || settings === void 0 ? void 0 : settings.canvasRenderer
        })
        this.canvasRenderer.addChangeListener(this.onSettingsChange.bind(this))
        this.canvasRenderer.mount(streamSection)

        // Canvas VSync (Canvas only: sync draw to display refresh to reduce tearing; off = lower latency)
        this.canvasVsync = new InputComponent("canvasVsync", "checkbox", i.canvasVsync, {
            checked: settings?.canvasVsync ?? defaultSettings_.canvasVsync
        })
        this.canvasVsync.addChangeListener(this.onSettingsChange.bind(this))
        this.canvasVsync.mount(streamSection)

        // HDR
        this.hdr = new InputComponent("hdr", "checkbox", i.enableHdr, {
            checked: settings?.hdr ?? defaultSettings_.hdr
        })
        this.hdr.addChangeListener(this.onSettingsChange.bind(this))
        this.hdr.mount(streamSection)

        if (!this.permissions.allow_hdr) {
            this.hdr.setChecked(false)
            this.hdr.setEnabled(false)
        }

        // Audio local
        const audioSection = this.newSection(this.audioHeader, i.audio)

        this.playAudioLocal = new InputComponent("playAudioLocal", "checkbox", i.playAudioLocal, {
            checked: settings?.playAudioLocal
        })
        this.playAudioLocal.addChangeListener(this.onSettingsChange.bind(this))
        this.playAudioLocal.mount(audioSection)

        // Audio Sample Queue Size
        this.audioSampleQueueSize = new InputComponent("audioSampleQueueSize", "number", i.audioSampleQueueSize, {
            defaultValue: defaultSettings_.audioSampleQueueSize.toString(),
            value: settings?.audioSampleQueueSize?.toString()
        })
        this.audioSampleQueueSize.addChangeListener(this.onSettingsChange.bind(this))
        this.audioSampleQueueSize.mount(audioSection)

        // Mouse
        const mouseSection = this.newSection(this.mouseHeader, i.mouse)

        this.mouseScrollMode = new SelectComponent("mouseScrollMode",
            [
                { value: "highres", name: i.highRes },
                { value: "normal", name: i.normal }
            ],
            {
                displayName: i.scrollMode,
                preSelectedOption: settings?.mouseScrollMode || defaultSettings_.mouseScrollMode
            }
        )
        this.mouseScrollMode.addChangeListener(this.onSettingsChange.bind(this))
        this.mouseScrollMode.mount(mouseSection)

        this.mouseMode = new SelectComponent("mouseMode",
            [
                { value: "relative", name: streamI.relative },
                { value: "follow", name: streamI.follow },
                { value: "localCursor", name: streamI.localCursor },
                { value: "pointAndDrag", name: streamI.pointAndDrag },
                // M4 cursor P1: host-authority auto mode. No translation key
                // exists yet — literal string per every-language convention
                // used elsewhere for untranslated technical labels.
                { value: "auto", name: "auto (host-controlled)" }
            ],
            {
                displayName: i.startupMouseMode,
                preSelectedOption: settings?.mouseMode ?? defaultSettings_.mouseMode
            }
        )
        this.mouseMode.addChangeListener(this.onSettingsChange.bind(this))
        this.mouseMode.mount(mouseSection)

        this.touchMode = new SelectComponent("touchMode",
            [
                { value: "touch", name: streamI.touch },
                { value: "mouseRelative", name: streamI.relative },
                { value: "localCursor", name: streamI.localCursor },
                { value: "pointAndDrag", name: streamI.pointAndDrag }
            ],
            {
                displayName: i.startupTouchMode,
                preSelectedOption: settings?.touchMode ?? defaultSettings_.touchMode
            }
        )
        this.touchMode.addChangeListener(this.onSettingsChange.bind(this))
        this.touchMode.mount(mouseSection)

        this.localCursorSensitivity = new InputComponent("localCursorSensitivity", "number", i.localCursorSensitivity, {
            defaultValue: defaultSettings_.localCursorSensitivity.toString(),
            value: settings?.localCursorSensitivity?.toString(),
            step: "0.1",
            numberSlider: {
                range_min: 0.1,
                range_max: 3
            }
        })
        this.localCursorSensitivity.addChangeListener(this.onSettingsChange.bind(this))
        this.localCursorSensitivity.mount(mouseSection)

        // Controller
        if (window.isSecureContext) {
            this.controllerHeader.innerText = i.controller
        } else {
            this.controllerHeader.innerText = i.controllerDisabled
        }
        const controllerSection = document.createElement("div")
        controllerSection.classList.add("settings-section")
        controllerSection.appendChild(this.controllerHeader)
        this.divElement.appendChild(controllerSection)

        this.controllerInvertAB = new InputComponent("controllerInvertAB", "checkbox", i.invertAB, {
            checked: settings?.controllerConfig?.invertAB
        })
        this.controllerInvertAB.addChangeListener(this.onSettingsChange.bind(this))
        this.controllerInvertAB.mount(controllerSection)

        this.controllerInvertXY = new InputComponent("controllerInvertXY", "checkbox", i.invertXY, {
            checked: settings?.controllerConfig?.invertXY
        })
        this.controllerInvertXY.addChangeListener(this.onSettingsChange.bind(this))
        this.controllerInvertXY.mount(controllerSection)

        // Controller Send Interval
        this.controllerSendIntervalOverride = new InputComponent("controllerSendIntervalOverride", "number", i.overrideControllerInterval, {
            hasEnableCheckbox: true,
            defaultValue: "20",
            value: settings?.controllerConfig?.sendIntervalOverride?.toString(),
            numberSlider: {
                range_min: 10,
                range_max: 120
            }
        })
        this.controllerSendIntervalOverride.setEnabled(settings?.controllerConfig?.sendIntervalOverride != null)
        this.controllerSendIntervalOverride.addChangeListener(this.onSettingsChange.bind(this))
        this.controllerSendIntervalOverride.mount(controllerSection)

        if (!window.isSecureContext) {
            this.controllerInvertAB.setEnabled(false)
            this.controllerInvertXY.setEnabled(false)
        }

        // Other
        const otherSection = this.newSection(this.otherHeader, i.other)

        // Data Transport
        const allowedDataTransport = [
            { value: "auto", name: i.auto },
        ]
        if (this.permissions.allow_transport_webrtc) {
            allowedDataTransport.push(
                { value: "webrtc", name: "WebRTC" },
            )
        }
        if (this.permissions.allow_transport_websockets) {
            allowedDataTransport.push(
                { value: "websocket", name: i.webSocket },
            )
        }

        this.language = new SelectComponent("language", getLanguageOptions(), {
            displayName: i.language,
            preSelectedOption: language
        })
        this.language.addChangeListener(this.onSettingsChange.bind(this))
        this.language.mount(otherSection)

        this.dataTransport = new SelectComponent("transport", allowedDataTransport, {
            displayName: i.dataTransport,
            preSelectedOption: settings?.dataTransport ?? defaultSettings_.dataTransport
        })
        this.dataTransport.addChangeListener(this.onSettingsChange.bind(this))
        this.dataTransport.mount(otherSection)

        this.enterFullscreenOnStreamStart = new InputComponent("enterFullscreenOnStreamStart", "checkbox", i.enterFullscreenOnStreamStart, {
            checked: settings?.enterFullscreenOnStreamStart ?? defaultSettings_.enterFullscreenOnStreamStart
        })
        this.enterFullscreenOnStreamStart.addChangeListener(this.onSettingsChange.bind(this))
        this.enterFullscreenOnStreamStart.mount(otherSection)

        // Fullscreen Keybind
        this.toggleFullscreenWithKeybind = new InputComponent("toggleFullscreenWithKeybind", "checkbox", i.toggleFullscreenWithKeybind, {
            checked: settings?.toggleFullscreenWithKeybind
        })
        this.toggleFullscreenWithKeybind.addChangeListener(this.onSettingsChange.bind(this))
        this.toggleFullscreenWithKeybind.mount(otherSection)

        // Page Style
        this.pageStyle = new SelectComponent("pageStyle", [
            { value: "standard", name: "Standard" },
            { value: "moonlight", name: "Moonlight" },
        ], {
            displayName: i.style,
            preSelectedOption: settings?.pageStyle ?? defaultSettings_.pageStyle
        })
        this.pageStyle.addChangeListener(this.onSettingsChange.bind(this))
        this.pageStyle.mount(otherSection)

        // Custom Select Element
        this.useSelectElementPolyfill = new InputComponent("useSelectElementPolyfill", "checkbox", i.useCustomDropdown, {
            checked: settings?.useSelectElementPolyfill ?? defaultSettings_.useSelectElementPolyfill
        })
        this.useSelectElementPolyfill.addChangeListener(this.onSettingsChange.bind(this))
        this.useSelectElementPolyfill.mount(otherSection)
        // Clipboard sync v1 (text-only, research/05 gap vs Parsec/DCV) — no
        // translation key exists yet, literal string per convention used
        // elsewhere for untranslated technical labels (see mouseMode "auto").
        this.clipboardSync = new InputComponent("clipboardSync", "checkbox", "Clipboard sync", {
            checked: settings?.clipboardSync ?? defaultSettings_.clipboardSync
        })
        this.clipboardSync.addChangeListener(this.onSettingsChange.bind(this))
        this.clipboardSync.mount(otherSection)
        // M4 cursor P2 (cursor-channel.md §P2) — experimental, no
        // translation key yet, same literal-label convention as clipboardSync.
        this.clientCursor = new InputComponent("clientCursor", "checkbox", "Client-rendered cursor (experimental)", {
            checked: settings?.clientCursor ?? defaultSettings_.clientCursor
        })
        this.clientCursor.addChangeListener(this.onSettingsChange.bind(this))
        this.clientCursor.mount(otherSection)

        this.onSettingsChange()
    }

    private onSettingsChange() {
        if (this.videoSize.getValue() == "custom") {
            this.videoSizeWidth.setEnabled(true)
            this.videoSizeHeight.setEnabled(true)
        } else {
            this.videoSizeWidth.setEnabled(false)
            this.videoSizeHeight.setEnabled(false)
        }

        this.divElement.dispatchEvent(new ComponentEvent("ml-settingschange", this))
    }

    addChangeListener(listener: StreamSettingsChangeListener) {
        this.divElement.addEventListener("ml-settingschange", listener as any)
    }
    removeChangeListener(listener: StreamSettingsChangeListener) {
        this.divElement.removeEventListener("ml-settingschange", listener as any)
    }

    getStreamSettings(): Settings {
        const settings = globalDefaultSettings()

        settings.sidebarEdge = this.sidebarEdge.getValue() as any
        settings.bitrate = parseInt(this.bitrate.getValue())
        settings.fps = parseInt(this.fps.getValue())
        settings.videoSize = this.videoSize.getValue() as any
        settings.videoSizeCustom = {
            width: parseInt(this.videoSizeWidth.getValue()),
            height: parseInt(this.videoSizeHeight.getValue())
        }
        settings.videoFrameQueueSize = parseInt(this.videoSampleQueueSize.getValue())
        settings.videoCodec = this.videoCodec.getValue() as any
        settings.forceVideoElementRenderer = this.forceVideoElementRenderer.isChecked()
        settings.canvasRenderer = this.canvasRenderer.isChecked()
        settings.canvasVsync = this.canvasVsync.isChecked()

        settings.playAudioLocal = this.playAudioLocal.isChecked()
        settings.audioSampleQueueSize = parseInt(this.audioSampleQueueSize.getValue())

        settings.mouseScrollMode = this.mouseScrollMode.getValue() as any
        settings.mouseMode = this.mouseMode.getValue() as MouseMode
        settings.touchMode = this.touchMode.getValue() as TouchMode
        settings.localCursorSensitivity = parseFloat(this.localCursorSensitivity.getValue())

        settings.controllerConfig.invertAB = this.controllerInvertAB.isChecked()
        settings.controllerConfig.invertXY = this.controllerInvertXY.isChecked()
        if (this.controllerSendIntervalOverride.isEnabled()) {
            settings.controllerConfig.sendIntervalOverride = parseInt(this.controllerSendIntervalOverride.getValue())
        } else {
            settings.controllerConfig.sendIntervalOverride = null
        }

        settings.dataTransport = this.dataTransport.getValue() as any
        settings.language = this.language.getValue() as Language

        settings.enterFullscreenOnStreamStart = this.enterFullscreenOnStreamStart.isChecked()
        settings.toggleFullscreenWithKeybind = this.toggleFullscreenWithKeybind.isChecked()

        settings.pageStyle = this.pageStyle.getValue() as any

        settings.hdr = this.hdr.isChecked()

        settings.useSelectElementPolyfill = this.useSelectElementPolyfill.isChecked()
        settings.clipboardSync = this.clipboardSync.isChecked()
        settings.clientCursor = this.clientCursor.isChecked()

        makeSettingsValid(this.permissions, settings)

        return settings
    }

    mountBefore(parent: HTMLElement, before: HTMLElement): void {
        parent.insertBefore(this.divElement, before)
    }
    mount(parent: HTMLElement): void {
        parent.appendChild(this.divElement)
    }
    unmount(parent: HTMLElement): void {
        parent.removeChild(this.divElement)
    }
}
