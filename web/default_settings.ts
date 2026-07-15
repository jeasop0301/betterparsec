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
    "enableVideoFec": false,
    // U4 P1 — QU lossless overlay test mode, default off (zero hot-path cost when false)
    "enableVideoQu": false,
    // Clipboard sync v1 — default true (Parsec ships this default-on; the
    // browser's own permission prompt is the actual consent gate).
    "clipboardSync": true,
    // M4 cursor P2 (cursor-channel.md §P2) — default false: Sunshine still
    // bakes the cursor into the video until the display_cursor fork config
    // lands, so this is off by default (identical behavior to before P2).
    "clientCursor": false
}

export default trueDefaultSettings as Settings
