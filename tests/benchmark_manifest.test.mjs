import assert from "node:assert/strict"
import test from "node:test"

import {
    sanitizeBenchmarkPageUrl,
    StreamStats,
} from "../dist/stream/stats.js"

const context = {
    pageUrl: "https://user:secret@example.test/prefix/stream.html?hostId=123&appId=456&token=secret#private",
    userAgent: "Benchmark Browser/1.0",
    streamSettings: {
        bitrateKbps: 20_000,
        width: 1920,
        height: 1080,
        fps: 60,
        requestedVideoCodec: "h264",
        requestedHdr: false,
        requestedDataTransport: "webrtc",
        iceTransportPolicy: "all",
        playAudioLocal: false,
        videoFrameQueueSize: 3,
        audioSampleQueueSize: 5,
    },
}

test("benchmark page URL strips credentials, query, and fragment", () => {
    assert.equal(
        sanitizeBenchmarkPageUrl(context.pageUrl),
        "https://example.test/prefix/stream.html",
    )
    assert.equal(sanitizeBenchmarkPageUrl("not a URL"), "unknown")
})

test("schema v2 manifest records reproducible browser-known context without guessing", () => {
    const stats = new StreamStats(undefined, context)
    stats.resetBenchmark()
    const report = stats.getBenchmarkReport()

    assert.equal(report.schemaVersion, 2)
    assert.equal(report.page.url, "https://example.test/prefix/stream.html")
    assert.equal(report.page.queryParametersIncluded, false)
    assert.equal(report.browser.userAgent, context.userAgent)
    assert.deepEqual(report.streamSettings, context.streamSettings)
    assert.equal(report.application.version, "unknown")
    assert.equal(report.application.build, "unknown")
    assert.equal(report.application.gitSha, "unknown")
    assert.equal(report.host.softwareVersion, "unknown")
    assert.equal(report.host.gpuDriverVersion, "unknown")
    assert.equal(report.host.gitSha, "unknown")
    assert.match(report.startedAt, /^\d{4}-\d{2}-\d{2}T/)
    assert.match(report.endedAt, /^\d{4}-\d{2}-\d{2}T/)
    assert.equal(report.exportedAt, report.endedAt)
    assert.deepEqual(report.sampleSequence, {
        first: null,
        last: null,
        count: 0,
        retainedLimit: 3600,
    })
})

test("samples carry monotonic sequence numbers and explicit source freshness", () => {
    const stats = new StreamStats(undefined, context)
    stats.resetBenchmark()

    // TypeScript private members compile to ordinary properties/methods. Avoid
    // starting the interval timer while exercising the deterministic recorder.
    stats.enabled = true
    stats.captureBenchmarkSample()
    stats.captureBenchmarkSample()

    const report = stats.getBenchmarkReport()
    assert.deepEqual(report.samples.map(sample => sample.sequence), [0, 1])
    assert.deepEqual(report.sampleSequence, {
        first: 0,
        last: 1,
        count: 2,
        retainedLimit: 3600,
    })
    for (const sample of report.samples) {
        assert.deepEqual(sample.freshnessMs, {
            transport: null,
            streamerRtt: null,
            streamerVideo: null,
            streamerVideoTransport: null,
            streamerBrowserRtt: null,
            runtimeBitrateControl: null,
            video: null,
            audio: null,
        })
    }
})

test("video transport keeps dequeue separate from actual track-write outcomes", () => {
    const stats = new StreamStats(undefined, context)
    stats.onMessage({
        VideoTransport: {
            interval_ms: 1000,
            queue_capacity_frames: 3,
            queue_depth_frames: 1,
            queue_max_depth_frames: 2,
            in_flight_frames: 1,
            in_flight_max_frames: 2,
            encoded_frames_received: 1,
            encoded_payload_bytes_received: 1000,
            frames_accepted: 1,
            frames_rejected: 0,
            frames_replaced: 0,
            frames_cleared: 0,
            frames_dropped: 0,
            frames_dequeued: 1,
            idr_frames_received: 0,
            idr_encoded_payload_bytes_received: 0,
            idr_frames_accepted: 0,
            idr_rtp_payload_bytes_accepted: 0,
            rtp_packets_dequeued: 3,
            rtp_payload_bytes_dequeued: 900,
            rtp_packets_write_succeeded: 1,
            rtp_payload_bytes_write_succeeded: 400,
            rtp_packets_write_failed: 1,
            rtp_payload_bytes_write_failed: 200,
            rtp_packets_write_skipped: 1,
            rtp_payload_bytes_write_skipped: 300,
            queue_wait_samples: 1,
            queue_wait_min_ms: 0.1,
            queue_wait_max_ms: 0.1,
            queue_wait_avg_ms: 0.1,
            rtp_write_latency_samples: 2,
            rtp_write_latency_min_ms: 0.2,
            rtp_write_latency_max_ms: 0.4,
            rtp_write_latency_avg_ms: 0.3,
        },
    })

    const transport = stats.statsData.streamerVideoTransport
    assert.equal(transport.rtpDequeuedPayloadBitrateKbps, 7.2)
    assert.equal(transport.rtpWriteSucceededPayloadBitrateKbps, 3.2)
    assert.equal(transport.rtpPacketsWriteFailed, 1)
    assert.equal(transport.rtpPacketsWriteSkipped, 1)
    assert.equal(transport.inFlightMaxFrames, 2)
    assert.equal(transport.queueWaitAvgMs, 0.1)
    assert.equal(transport.rtpWriteLatencyAvgMs, 0.3)
})
