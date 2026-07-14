import test from "node:test";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import { analyzeRun, formatReport } from "./analyze-benchmark.mjs";

function profile(overrides = {}) {
    return {
        schemaVersion: 1,
        name: "1080p60-h264",
        stream: {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: "h264",
            hdr: false,
        },
        timing: {
            measurementSeconds: 2,
        },
        validityGates: {
            requiredIceRoute: "direct",
            requiredCandidateProtocol: "udp",
            rejectIcePathTransitions: true,
            maximumSourceFreshnessMs: 2500,
            maximumSenderQueueFrames: 2,
            maximumHealthyLinkFrameRejectionPercent: 0.5,
            maximumBrowserDecodeP95Ms: 5,
            requireZeroHealthyLinkFreezes: true,
            requireZeroSenderWriteFailures: true,
            ...overrides,
        },
    };
}

function sample(sequence, overrides = {}) {
    const base = {
        sequence,
        elapsedMs: (sequence + 1) * 1000,
        collectedAt: new Date(1_700_000_000_000 + sequence * 1000).toISOString(),
        freshnessMs: {
            transport: 5,
            streamerVideo: 500,
            streamerVideoTransport: 500,
        },
        stats: {
            videoCodec: "H264",
            transport: {
                webrtcSelectedCandidatePairId: "pair-1",
                webrtcIceRoute: "direct",
                webrtcLocalCandidateType: "host",
                webrtcLocalCandidateProtocol: "udp",
                webrtcRemoteCandidateType: "host",
                webrtcRemoteCandidateProtocol: "udp",
                webrtcCandidatePairCurrentRttMs: 12,
                webrtcFps: 60,
                webrtcJitterMs: 2,
                webrtcAvgDecodeTimeMs: 0.7 + sequence * 0.1,
                webrtcAvgProcessingDelayMs: 1.2,
                webrtcPayloadReceiveBitrateKbps: 8000,
                webrtcPacketLossPercent: 0,
                webrtcPacketsLostDelta: 0,
                webrtcFramesDroppedDelta: 0,
                webrtcFreezeCountDelta: 0,
                webrtcNackCountDelta: 0,
                webrtcPliCountDelta: 0,
            },
            streamerVideoTransport: {
                queueDepthFrames: 0,
                queueMaxDepthFrames: 1,
                inFlightMaxFrames: 1,
                encodedFramesReceived: 60,
                framesAccepted: 60,
                framesRejected: 0,
                framesReplaced: 0,
                framesCleared: 0,
                framesDropped: 0,
                queueWaitAvgMs: 0.08,
                rtpWriteLatencyAvgMs: 0.25,
                rtpPacketsWriteSucceeded: 120,
                rtpPacketsWriteFailed: 0,
                rtpPacketsWriteSkipped: 0,
                encodedInputBitrateKbps: 8000,
                rtpWriteSucceededPayloadBitrateKbps: 7900,
            },
        },
    };
    return deepMerge(base, overrides);
}

function deepMerge(base, override) {
    if (typeof base !== "object" || base === null || Array.isArray(base)) return override;
    const output = { ...base };
    for (const [key, value] of Object.entries(override)) {
        output[key] = typeof value === "object" && value !== null && !Array.isArray(value)
            ? deepMerge(base[key] ?? {}, value)
            : value;
    }
    return output;
}

async function createRun({ profileValue = profile(), samples = [sample(0), sample(1), sample(2)], manifest = {}, includeBrowser = true } = {}) {
    const root = await mkdtemp(path.join(os.tmpdir(), "betterparsec-analysis-"));
    const manifestValue = deepMerge({
        schemaVersion: 1,
        runId: "test-run",
        candidate: "BetterParsec",
        state: "completed",
        repository: { head: "abc123", dirty: false, status: [] },
        startedAt: new Date(1_700_000_000_000).toISOString(),
        endedAt: new Date(1_700_000_010_000).toISOString(),
        networkTrace: { applied: true },
        packetCapture: { enabled: true },
        contentTrace: { sha256: "fixture-content-sha256" },
        browserExport: { artifact: "browser-benchmark.json", status: "collected" },
    }, manifest);
    await writeFile(path.join(root, "manifest.json"), JSON.stringify(manifestValue), "utf8");
    await writeFile(path.join(root, "profile.json"), JSON.stringify(profileValue), "utf8");

    if (includeBrowser) {
        const browser = {
            schemaVersion: 2,
            startedAt: new Date(1_700_000_000_500).toISOString(),
            endedAt: new Date(1_700_000_004_000).toISOString(),
            sampleWindowStartedAt: new Date(1_700_000_001_000).toISOString(),
            sampleWindowEndedAt: new Date(1_700_000_003_000).toISOString(),
            streamSettings: {
                width: 1920,
                height: 1080,
                fps: 60,
                requestedVideoCodec: "h264",
                requestedHdr: false,
            },
            selected: { videoCodec: "H264" },
            sampleSequence: {
                first: samples[0]?.sequence ?? null,
                last: samples.at(-1)?.sequence ?? null,
                count: samples.length,
                retainedLimit: 3600,
            },
            samples,
        };
        await writeFile(path.join(root, "browser-benchmark.json"), JSON.stringify(browser), "utf8");
    }
    return root;
}

test("accepts a valid direct UDP run and reports metric distributions", async () => {
    const root = await createRun();
    const report = await analyzeRun(root);

    assert.equal(report.accepted, true);
    assert.equal(report.verdict, "accepted");
    assert.deepEqual(report.errors, []);
    assert.equal(report.metrics.sampleCount, 3);
    assert.equal(report.metrics.sender.encodedFrames, 180);
    assert.equal(report.metrics.sender.queueWaitAvgMs.p95, 0.08);
    assert(Math.abs(report.metrics.browser.decodeTimeMs.p95 - 0.9) < 1e-9);
    assert.match(formatReport(report), /^ACCEPT test-run/);
});

test("rejects non-contiguous browser sample sequences", async () => {
    const root = await createRun({ samples: [sample(0), sample(2)] });
    const report = await analyzeRun(root);

    assert.equal(report.accepted, false);
    assert(report.errors.some((entry) => entry.code === "sequence-gap"));
});

test("rejects a route or candidate protocol that differs from the profile", async () => {
    const root = await createRun({
        samples: [sample(0, {
            stats: {
                transport: {
                    webrtcIceRoute: "relay",
                    webrtcLocalCandidateProtocol: "tcp",
                    webrtcRemoteCandidateProtocol: "tcp",
                },
            },
        })],
    });
    const report = await analyzeRun(root);

    assert.equal(report.accepted, false);
    assert(report.errors.some((entry) => entry.code === "ice-route-mismatch"));
    assert(report.errors.some((entry) => entry.code === "candidate-protocol-mismatch"));
});

test("rejects queue, frame rejection, write failure, decode, and freeze regressions", async () => {
    const root = await createRun({
        samples: [sample(0, {
            stats: {
                transport: {
                    webrtcAvgDecodeTimeMs: 8,
                    webrtcFreezeCountDelta: 1,
                },
                streamerVideoTransport: {
                    queueMaxDepthFrames: 3,
                    encodedFramesReceived: 100,
                    framesRejected: 2,
                    rtpPacketsWriteFailed: 1,
                },
            },
        })],
    });
    const report = await analyzeRun(root);
    const codes = new Set(report.errors.map((entry) => entry.code));

    assert.equal(report.accepted, false);
    for (const code of [
        "sender-queue-gate",
        "frame-rejection-gate",
        "sender-write-failure",
        "browser-decode-gate",
        "browser-freeze-gate",
    ]) {
        assert(codes.has(code), `expected rejection code ${code}`);
    }
});

test("rejects a missing browser artifact and warns about a dirty source tree", async () => {
    const root = await createRun({
        includeBrowser: false,
        manifest: { repository: { dirty: true, status: [" M streamer/src/main.rs"] } },
    });
    const report = await analyzeRun(root);

    assert.equal(report.accepted, false);
    assert(report.errors.some((entry) => entry.code === "browser-export-missing"));
    assert(report.warnings.some((entry) => entry.code === "repository-dirty"));
});

test("rejects a browser artifact from another run and an undersized measurement window", async () => {
    const root = await createRun({
        profileValue: profile({ minimumMeasuredDurationPercent: 100 }),
        samples: [sample(0), sample(1)],
        manifest: {
            startedAt: new Date(1_700_100_000_000).toISOString(),
            endedAt: new Date(1_700_100_010_000).toISOString(),
        },
    });
    const report = await analyzeRun(root);

    assert.equal(report.accepted, false);
    assert(report.errors.some((entry) => entry.code === "measurement-too-short"));
    assert(report.errors.some((entry) => entry.code === "artifact-time-mismatch"));
});

test("supports explicit FPS, processing, and jitter-buffer latency gates", async () => {
    const root = await createRun({
        profileValue: profile({
            minimumBrowserFpsP50: 55,
            maximumBrowserProcessingP95Ms: 3,
            maximumJitterBufferTargetP95Ms: 20,
            maximumJitterBufferMinimumP95Ms: 15,
        }),
        samples: [sample(0, {
            stats: {
                transport: {
                    webrtcFps: 40,
                    webrtcAvgProcessingDelayMs: 4,
                    webrtcJitterBufferTargetDelayMs: 42,
                    webrtcJitterBufferMinimumDelayMs: 40,
                },
            },
        }), sample(1, { stats: { transport: { webrtcFps: 40 } } }), sample(2)],
    });
    const report = await analyzeRun(root);
    const codes = new Set(report.errors.map((entry) => entry.code));

    assert.equal(report.accepted, false);
    for (const code of [
        "browser-fps-gate",
        "browser-processing-gate",
        "jitter-buffer-target-gate",
        "jitter-buffer-minimum-gate",
    ]) {
        assert(codes.has(code), `expected rejection code ${code}`);
    }
});

test("strict comparison gates require clean source, content hash, and verified PCAPNG", async () => {
    const root = await createRun({
        profileValue: profile({
            requireCleanRepository: true,
            requireContentTraceHash: true,
            requirePacketCapture: true,
        }),
        manifest: {
            repository: { dirty: true, status: [" M streamer/src/main.rs"] },
            contentTrace: { sha256: null },
            packetCapture: {
                enabled: true,
                pcapng: "capture/wire.pcapng",
                pcapngSha256: null,
            },
        },
    });
    const report = await analyzeRun(root);
    const codes = new Set(report.errors.map((entry) => entry.code));

    assert.equal(report.accepted, false);
    for (const code of [
        "repository-dirty",
        "content-trace-unpinned",
        "packet-capture-artifact-missing",
    ]) {
        assert(codes.has(code), `expected rejection code ${code}`);
    }
});

test("rejects a PCAPNG whose bytes do not match the manifest SHA-256", async () => {
    const root = await createRun({
        profileValue: profile({ requirePacketCapture: true }),
        manifest: {
            packetCapture: {
                enabled: true,
                pcapng: "capture/wire.pcapng",
                pcapngSha256: "00".repeat(32),
            },
        },
    });
    await mkdir(path.join(root, "capture"), { recursive: true });
    await writeFile(path.join(root, "capture", "wire.pcapng"), "fixture-pcap", "utf8");

    const report = await analyzeRun(root);
    assert.equal(report.accepted, false);
    assert(report.errors.some((entry) => entry.code === "packet-capture-hash-mismatch"));
});

test("rejects stale telemetry and ICE pair transitions", async () => {
    const root = await createRun({
        samples: [
            sample(0),
            sample(1, {
                freshnessMs: { streamerVideoTransport: 3000 },
                stats: { transport: { webrtcSelectedCandidatePairId: "pair-2" } },
            }),
        ],
    });
    const report = await analyzeRun(root);

    assert.equal(report.accepted, false);
    assert(report.errors.some((entry) => entry.code === "source-stale"));
    assert(report.errors.some((entry) => entry.code === "ice-path-transition"));
});
