import { Settings } from "./component/settings_menu.js"
import CONFIG from "./config.js"

const trueDefaultSettings: Settings =

// When updated, update the README
{
    // possible values: "left", "right", "up", "down"
    "sidebarEdge": "left",
    "bitrate": 10000,
    "fps": 60,
    "videoFrameQueueSize": 3,
    // possible values: "720p", "1080p", "1440p", "4k", "native", "custom"
    "videoSize": "custom",
    // only works if videoSize=custom
    "videoSizeCustom": {
        "width": 1920,
        "height": 1080
    },
    // possible values: "h264", "h265", "av1", "auto"
    "videoCodec": "h264",
    "forceVideoElementRenderer": false,
    "canvasRenderer": false,
    // Canvas only: when true, draw only on requestAnimationFrame (stable, may add ~0–17 ms). When false, draw on frame submit (low latency).
    "canvasVsync": false,
    "playAudioLocal": false,
    "audioSampleQueueSize": 20,
    // possible values: "highres", "normal"
    "mouseScrollMode": "highres",
    // possible values: "relative", "follow", "pointAndDrag"
    "mouseMode": "follow",
    // possible values: "touch", "mouseRelative", "localCursor", "pointAndDrag"
    "touchMode": "mouseRelative",
    "localCursorSensitivity": 1,
    "controllerConfig": {
        "invertAB": false,
        "invertXY": false,
        // possible values: null or a number, example: 60, 120
        "sendIntervalOverride": null
    },
    // possible values: "auto", "webrtc", "websocket"
    "dataTransport": "auto",
    // possible values: "all", "relay" — "relay" forces every WebRTC candidate
    // through the TURN relay (443/TLS), for locked networks that block direct/UDP.
    "iceTransportPolicy": "all",
    "language": "en",
    "enterFullscreenOnStreamStart": false,
    "toggleFullscreenWithKeybind": false,
    // possible values: "standard", "old"
    "pageStyle": "standard",
    "hdr": false,
    "useSelectElementPolyfill": false,
    // U2 P1 — FEC pipeline test mode, default off (zero hot-path cost when false)
    "enableVideoFec": false
}

export default trueDefaultSettings as Settings
