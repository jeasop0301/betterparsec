#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFileSync, existsSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

const DEFAULT_MAX_SOURCE_FRESHNESS_MS = 2500;

function get(object, dottedPath) {
    return dottedPath.split(".").reduce((value, key) => value?.[key], object);
}

function finiteNumber(value) {
    return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function nearestRank(values, percentile) {
    const sorted = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
    if (sorted.length === 0) {
        return null;
    }
    const rank = Math.max(1, Math.ceil((percentile / 100) * sorted.length));
    return sorted[rank - 1];
}

function distribution(values) {
    const finite = values.filter((value) => Number.isFinite(value));
    if (finite.length === 0) {
        return { count: 0, min: null, p50: null, p95: null, p99: null, max: null };
    }
    return {
        count: finite.length,
        min: Math.min(...finite),
        p50: nearestRank(finite, 50),
        p95: nearestRank(finite, 95),
        p99: nearestRank(finite, 99),
        max: Math.max(...finite),
    };
}

function sum(values) {
    return values.reduce((total, value) => total + (finiteNumber(value) ?? 0), 0);
}

function maximum(values) {
    const finite = values.filter((value) => Number.isFinite(value));
    return finite.length === 0 ? null : Math.max(...finite);
}

function uniquePresent(values) {
    return [...new Set(values.filter((value) => value !== undefined && value !== null && value !== ""))];
}

function normalizeCodec(codec) {
    const normalized = String(codec ?? "").toLowerCase().replaceAll(/[^a-z0-9]/g, "");
    if (normalized.startsWith("h264") || normalized.startsWith("avc")) return "h264";
    if (normalized.startsWith("h265") || normalized.startsWith("hevc")) return "h265";
    if (normalized.startsWith("av1") || normalized.startsWith("av01")) return "av1";
    return normalized;
}

function sha256File(filePath) {
    return createHash("sha256").update(readFileSync(filePath)).digest("hex");
}

function issue(code, message, evidence = undefined) {
    return evidence === undefined ? { code, message } : { code, message, evidence };
}

function sampleValues(samples, dottedPath) {
    return samples.map((sample) => get(sample, dottedPath)).filter((value) => value !== undefined);
}

function sumDeltaOrFinal(samples, deltaPath, cumulativePath) {
    const deltas = sampleValues(samples, deltaPath).filter((value) => Number.isFinite(value));
    if (deltas.length > 0) {
        return sum(deltas);
    }
    const cumulative = sampleValues(samples, cumulativePath).filter((value) => Number.isFinite(value));
    return cumulative.length === 0 ? null : cumulative.at(-1);
}

function validateSequence(samples, errors) {
    if (samples.length === 0) {
        errors.push(issue("samples-empty", "Browser benchmark contains no samples."));
        return;
    }

    let previousSequence = null;
    let previousElapsedMs = null;
    for (const [index, sample] of samples.entries()) {
        if (!Number.isInteger(sample.sequence)) {
            errors.push(issue("sequence-invalid", `Sample ${index} has no integer sequence.`, { value: sample.sequence }));
            continue;
        }
        if (previousSequence !== null && sample.sequence !== previousSequence + 1) {
            errors.push(issue("sequence-gap", "Browser sample sequence is not contiguous.", {
                previous: previousSequence,
                current: sample.sequence,
            }));
        }
        if (!Number.isFinite(sample.elapsedMs)) {
            errors.push(issue("elapsed-invalid", `Sample ${sample.sequence} has invalid elapsedMs.`, { value: sample.elapsedMs }));
        } else if (previousElapsedMs !== null && sample.elapsedMs <= previousElapsedMs) {
            errors.push(issue("elapsed-not-monotonic", "Browser sample elapsed time did not increase.", {
                previous: previousElapsedMs,
                current: sample.elapsedMs,
                sequence: sample.sequence,
            }));
        }
        previousSequence = sample.sequence;
        previousElapsedMs = sample.elapsedMs;
    }
}

function parseTimestamp(value) {
    const timestamp = Date.parse(value ?? "");
    return Number.isFinite(timestamp) ? timestamp : null;
}

function validateRunEnvelope(manifest, profile, browser, samples, errors, warnings, runRoot) {
    const gates = profile.validityGates ?? {};
    const expectedMeasurementSeconds = finiteNumber(profile.timing?.measurementSeconds);
    const firstElapsedMs = finiteNumber(samples[0]?.elapsedMs);
    const lastElapsedMs = finiteNumber(samples.at(-1)?.elapsedMs);
    const measuredDurationMs = firstElapsedMs !== null && lastElapsedMs !== null
        ? Math.max(0, lastElapsedMs - firstElapsedMs)
        : null;

    if (expectedMeasurementSeconds !== null) {
        const minimumMeasuredDurationPercent = gates.minimumMeasuredDurationPercent ?? 95;
        const minimumDurationMs = expectedMeasurementSeconds * 1000 * minimumMeasuredDurationPercent / 100;
        if (measuredDurationMs === null || measuredDurationMs < minimumDurationMs) {
            errors.push(issue("measurement-too-short", "Browser sample window is shorter than the profile requires.", {
                expectedMeasurementSeconds,
                minimumMeasuredDurationPercent,
                minimumDurationMs,
                observedDurationMs: measuredDurationMs,
            }));
        }
    }

    const manifestStart = parseTimestamp(manifest.startedAt);
    const manifestEnd = parseTimestamp(manifest.endedAt);
    const browserStart = parseTimestamp(browser.sampleWindowStartedAt ?? browser.startedAt);
    const browserEnd = parseTimestamp(browser.sampleWindowEndedAt ?? browser.endedAt);
    const allowedClockSkewMs = gates.maximumArtifactClockSkewMs ?? 5000;

    if ([manifestStart, manifestEnd, browserStart, browserEnd].every((value) => value !== null)) {
        if (browserStart < manifestStart - allowedClockSkewMs || browserEnd > manifestEnd + allowedClockSkewMs) {
            errors.push(issue("artifact-time-mismatch", "Browser export does not belong to the run manifest time envelope.", {
                allowedClockSkewMs,
                manifestStartedAt: manifest.startedAt,
                manifestEndedAt: manifest.endedAt,
                browserStartedAt: browser.sampleWindowStartedAt ?? browser.startedAt,
                browserEndedAt: browser.sampleWindowEndedAt ?? browser.endedAt,
            }));
        }
    } else {
        warnings.push(issue("artifact-time-unverifiable", "Run/browser timestamps are incomplete, so artifact identity cannot be time-verified."));
    }

    if (gates.requireNetworkTraceApplied && manifest.networkTrace?.applied !== true) {
        errors.push(issue("network-trace-not-applied", "Profile requires deterministic network shaping, but the manifest says it was not applied."));
    } else if (manifest.networkTrace?.applied === false) {
        warnings.push(issue("network-trace-not-applied", "Network shaping was not applied for this run."));
    }

    if (gates.requirePacketCapture && manifest.packetCapture?.enabled !== true) {
        errors.push(issue("packet-capture-disabled", "Profile requires packet capture, but the manifest says it was disabled."));
    } else if (manifest.packetCapture?.enabled === false) {
        warnings.push(issue("packet-capture-disabled", "Packet capture was disabled for this run."));
    }

    if (gates.requirePacketCapture) {
        const pcapArtifact = manifest.packetCapture?.pcapng
            ? path.resolve(runRoot, manifest.packetCapture.pcapng)
            : null;
        const pcapExists = pcapArtifact !== null && existsSync(pcapArtifact);
        if (!pcapExists || !manifest.packetCapture?.pcapngSha256) {
            errors.push(issue("packet-capture-artifact-missing", "Packet capture was required but no verified PCAPNG artifact was recorded.", {
                artifact: manifest.packetCapture?.pcapng ?? null,
                artifactExists: pcapExists,
                sha256: manifest.packetCapture?.pcapngSha256 ?? null,
            }));
        } else {
            const actualSha256 = sha256File(pcapArtifact);
            const declaredSha256 = String(manifest.packetCapture.pcapngSha256).toLowerCase();
            if (actualSha256 !== declaredSha256) {
                errors.push(issue("packet-capture-hash-mismatch", "PCAPNG SHA-256 does not match the run manifest.", {
                    artifact: manifest.packetCapture.pcapng,
                    declaredSha256,
                    actualSha256,
                }));
            }
        }
    }

    if (!manifest.contentTrace?.sha256) {
        const contentIssue = issue("content-trace-unpinned", "No deterministic content-trace SHA-256 was recorded.");
        if (gates.requireContentTraceHash) {
            errors.push(contentIssue);
        } else {
            warnings.push(contentIssue);
        }
    }
}

function validateProfileAndBrowser(profile, browser, samples, errors, warnings) {
    const gates = profile.validityGates ?? {};
    const requested = browser.streamSettings ?? {};
    const selected = browser.selected ?? {};
    const expected = profile.stream ?? {};

    const expectedCodec = normalizeCodec(expected.codec);
    const observedCodecs = uniquePresent([
        selected.videoCodec,
        ...sampleValues(samples, "stats.videoCodec"),
    ]).map(normalizeCodec);
    if (expectedCodec && (observedCodecs.length === 0 || observedCodecs.some((codec) => codec !== expectedCodec))) {
        errors.push(issue("codec-mismatch", "Observed codec does not match the benchmark profile.", {
            expected: expectedCodec,
            observed: observedCodecs,
        }));
    }

    for (const [field, expectedValue] of [["width", expected.width], ["height", expected.height], ["fps", expected.fps]]) {
        if (Number.isFinite(expectedValue) && requested[field] !== expectedValue) {
            errors.push(issue(`stream-${field}-mismatch`, `Requested ${field} does not match the benchmark profile.`, {
                expected: expectedValue,
                observed: requested[field] ?? null,
            }));
        }
    }

    if (typeof expected.hdr === "boolean" && requested.requestedHdr !== expected.hdr) {
        errors.push(issue("stream-hdr-mismatch", "Requested HDR state does not match the benchmark profile.", {
            expected: expected.hdr,
            observed: requested.requestedHdr ?? null,
        }));
    }

    const routes = uniquePresent(sampleValues(samples, "stats.transport.webrtcIceRoute"));
    if (gates.requiredIceRoute && (routes.length !== 1 || routes[0] !== gates.requiredIceRoute)) {
        errors.push(issue("ice-route-mismatch", "Selected ICE route does not match the required route.", {
            required: gates.requiredIceRoute,
            observed: routes,
        }));
    }

    const candidatePairIds = uniquePresent(sampleValues(samples, "stats.transport.webrtcSelectedCandidatePairId"));
    const rejectIcePathTransitions = gates.rejectIcePathTransitions ?? true;
    if (rejectIcePathTransitions && candidatePairIds.length > 1) {
        errors.push(issue("ice-path-transition", "Selected ICE candidate pair changed during the measured run.", {
            candidatePairIds,
        }));
    }

    if (gates.requiredCandidateProtocol) {
        const localProtocols = uniquePresent(sampleValues(samples, "stats.transport.webrtcLocalCandidateProtocol"));
        const remoteProtocols = uniquePresent(sampleValues(samples, "stats.transport.webrtcRemoteCandidateProtocol"));
        const observedProtocols = uniquePresent([...localProtocols, ...remoteProtocols]);
        if (observedProtocols.length === 0 || observedProtocols.some((value) => value !== gates.requiredCandidateProtocol)) {
            errors.push(issue("candidate-protocol-mismatch", "ICE candidate protocol does not match the required protocol.", {
                required: gates.requiredCandidateProtocol,
                local: localProtocols,
                remote: remoteProtocols,
            }));
        }
    }

    const maximumSourceFreshnessMs = gates.maximumSourceFreshnessMs ?? DEFAULT_MAX_SOURCE_FRESHNESS_MS;
    for (const source of ["transport", "streamerVideo", "streamerVideoTransport"]) {
        const freshness = sampleValues(samples, `freshnessMs.${source}`).filter((value) => Number.isFinite(value));
        if (freshness.length === 0) {
            warnings.push(issue("freshness-missing", `No freshness samples were exported for ${source}.`, { source }));
            continue;
        }
        const maxFreshness = Math.max(...freshness);
        if (maxFreshness > maximumSourceFreshnessMs) {
            errors.push(issue("source-stale", `${source} telemetry exceeded the freshness gate.`, {
                source,
                maximumAllowedMs: maximumSourceFreshnessMs,
                observedMaximumMs: maxFreshness,
            }));
        }
    }

    const queueDepthMax = maximum(sampleValues(samples, "stats.streamerVideoTransport.queueMaxDepthFrames"));
    if (Number.isFinite(gates.maximumSenderQueueFrames) && (queueDepthMax === null || queueDepthMax > gates.maximumSenderQueueFrames)) {
        errors.push(issue("sender-queue-gate", "Sender queue depth exceeded the profile gate.", {
            maximumAllowedFrames: gates.maximumSenderQueueFrames,
            observedMaximumFrames: queueDepthMax,
        }));
    }

    const encodedFrames = sum(sampleValues(samples, "stats.streamerVideoTransport.encodedFramesReceived"));
    const rejectedFrames = sum(sampleValues(samples, "stats.streamerVideoTransport.framesRejected"));
    const rejectionPercent = encodedFrames > 0 ? (rejectedFrames / encodedFrames) * 100 : null;
    if (Number.isFinite(gates.maximumHealthyLinkFrameRejectionPercent)
        && (rejectionPercent === null || rejectionPercent > gates.maximumHealthyLinkFrameRejectionPercent)) {
        errors.push(issue("frame-rejection-gate", "Sender frame rejection exceeded the profile gate.", {
            maximumAllowedPercent: gates.maximumHealthyLinkFrameRejectionPercent,
            observedPercent: rejectionPercent,
            encodedFrames,
            rejectedFrames,
        }));
    }

    const writeFailures = sum(sampleValues(samples, "stats.streamerVideoTransport.rtpPacketsWriteFailed"));
    if ((gates.requireZeroSenderWriteFailures ?? true) && writeFailures > 0) {
        errors.push(issue("sender-write-failure", "At least one RTP track write failed during the run.", { writeFailures }));
    }

    const browserFpsDistribution = distribution(sampleValues(samples, "stats.transport.webrtcFps"));
    if (Number.isFinite(gates.minimumBrowserFpsP50)
        && (browserFpsDistribution.p50 === null || browserFpsDistribution.p50 < gates.minimumBrowserFpsP50)) {
        errors.push(issue("browser-fps-gate", "Browser FPS p50 fell below the profile gate.", {
            minimumRequiredFps: gates.minimumBrowserFpsP50,
            observedP50Fps: browserFpsDistribution.p50,
        }));
    }

    const decodeDistribution = distribution(sampleValues(samples, "stats.transport.webrtcAvgDecodeTimeMs"));
    if (Number.isFinite(gates.maximumBrowserDecodeP95Ms)
        && (decodeDistribution.p95 === null || decodeDistribution.p95 > gates.maximumBrowserDecodeP95Ms)) {
        errors.push(issue("browser-decode-gate", "Browser decode p95 exceeded the profile gate.", {
            maximumAllowedMs: gates.maximumBrowserDecodeP95Ms,
            observedP95Ms: decodeDistribution.p95,
        }));
    }

    const processingDistribution = distribution(sampleValues(samples, "stats.transport.webrtcAvgProcessingDelayMs"));
    if (Number.isFinite(gates.maximumBrowserProcessingP95Ms)
        && (processingDistribution.p95 === null || processingDistribution.p95 > gates.maximumBrowserProcessingP95Ms)) {
        errors.push(issue("browser-processing-gate", "Browser processing-delay p95 exceeded the profile gate.", {
            maximumAllowedMs: gates.maximumBrowserProcessingP95Ms,
            observedP95Ms: processingDistribution.p95,
        }));
    }

    const jitterBufferTargetDistribution = distribution(sampleValues(samples, "stats.transport.webrtcJitterBufferTargetDelayMs"));
    if (Number.isFinite(gates.maximumJitterBufferTargetP95Ms)
        && (jitterBufferTargetDistribution.p95 === null || jitterBufferTargetDistribution.p95 > gates.maximumJitterBufferTargetP95Ms)) {
        errors.push(issue("jitter-buffer-target-gate", "Jitter-buffer target-delay p95 exceeded the profile gate.", {
            maximumAllowedMs: gates.maximumJitterBufferTargetP95Ms,
            observedP95Ms: jitterBufferTargetDistribution.p95,
        }));
    }

    const jitterBufferMinimumDistribution = distribution(sampleValues(samples, "stats.transport.webrtcJitterBufferMinimumDelayMs"));
    if (Number.isFinite(gates.maximumJitterBufferMinimumP95Ms)
        && (jitterBufferMinimumDistribution.p95 === null || jitterBufferMinimumDistribution.p95 > gates.maximumJitterBufferMinimumP95Ms)) {
        errors.push(issue("jitter-buffer-minimum-gate", "Jitter-buffer minimum-delay p95 exceeded the profile gate.", {
            maximumAllowedMs: gates.maximumJitterBufferMinimumP95Ms,
            observedP95Ms: jitterBufferMinimumDistribution.p95,
        }));
    }

    const freezes = sumDeltaOrFinal(
        samples,
        "stats.transport.webrtcFreezeCountDelta",
        "stats.transport.webrtcFreezeCount",
    );
    if (gates.requireZeroHealthyLinkFreezes && freezes !== 0) {
        errors.push(issue("browser-freeze-gate", "Browser reported a freeze during a zero-freeze profile.", { freezes }));
    }

    const packetLossPercentMax = maximum(sampleValues(samples, "stats.transport.webrtcPacketLossPercent"));
    if (Number.isFinite(gates.maximumPacketLossPercent)
        && (packetLossPercentMax === null || packetLossPercentMax > gates.maximumPacketLossPercent)) {
        errors.push(issue("packet-loss-gate", "Browser packet loss exceeded the profile gate.", {
            maximumAllowedPercent: gates.maximumPacketLossPercent,
            observedMaximumPercent: packetLossPercentMax,
        }));
    }
}

function buildMetrics(samples) {
    const transportPath = "stats.streamerVideoTransport";
    const browserTransportPath = "stats.transport";
    const encodedFrames = sum(sampleValues(samples, `${transportPath}.encodedFramesReceived`));
    const framesRejected = sum(sampleValues(samples, `${transportPath}.framesRejected`));

    return {
        sampleCount: samples.length,
        measuredDurationMs: samples.length > 1
            ? samples.at(-1).elapsedMs - samples[0].elapsedMs
            : 0,
        sender: {
            encodedFrames,
            framesAccepted: sum(sampleValues(samples, `${transportPath}.framesAccepted`)),
            framesRejected,
            framesReplaced: sum(sampleValues(samples, `${transportPath}.framesReplaced`)),
            framesCleared: sum(sampleValues(samples, `${transportPath}.framesCleared`)),
            framesDropped: sum(sampleValues(samples, `${transportPath}.framesDropped`)),
            frameRejectionPercent: encodedFrames > 0 ? (framesRejected / encodedFrames) * 100 : null,
            queueDepthFrames: distribution(sampleValues(samples, `${transportPath}.queueDepthFrames`)),
            queueMaxDepthFrames: maximum(sampleValues(samples, `${transportPath}.queueMaxDepthFrames`)),
            inFlightMaxFrames: maximum(sampleValues(samples, `${transportPath}.inFlightMaxFrames`)),
            queueWaitAvgMs: distribution(sampleValues(samples, `${transportPath}.queueWaitAvgMs`)),
            rtpWriteLatencyAvgMs: distribution(sampleValues(samples, `${transportPath}.rtpWriteLatencyAvgMs`)),
            rtpPacketsWriteSucceeded: sum(sampleValues(samples, `${transportPath}.rtpPacketsWriteSucceeded`)),
            rtpPacketsWriteFailed: sum(sampleValues(samples, `${transportPath}.rtpPacketsWriteFailed`)),
            rtpPacketsWriteSkipped: sum(sampleValues(samples, `${transportPath}.rtpPacketsWriteSkipped`)),
            encodedInputBitrateKbps: distribution(sampleValues(samples, `${transportPath}.encodedInputBitrateKbps`)),
            writeSucceededPayloadBitrateKbps: distribution(sampleValues(samples, `${transportPath}.rtpWriteSucceededPayloadBitrateKbps`)),
        },
        browser: {
            fps: distribution(sampleValues(samples, `${browserTransportPath}.webrtcFps`)),
            currentRttMs: distribution(sampleValues(samples, `${browserTransportPath}.webrtcCandidatePairCurrentRttMs`)),
            jitterMs: distribution(sampleValues(samples, `${browserTransportPath}.webrtcJitterMs`)),
            jitterBufferDelayMs: distribution(sampleValues(samples, `${browserTransportPath}.webrtcJitterBufferDelayMs`)),
            jitterBufferTargetDelayMs: distribution(sampleValues(samples, `${browserTransportPath}.webrtcJitterBufferTargetDelayMs`)),
            decodeTimeMs: distribution(sampleValues(samples, `${browserTransportPath}.webrtcAvgDecodeTimeMs`)),
            processingDelayMs: distribution(sampleValues(samples, `${browserTransportPath}.webrtcAvgProcessingDelayMs`)),
            payloadReceiveBitrateKbps: distribution(sampleValues(samples, `${browserTransportPath}.webrtcPayloadReceiveBitrateKbps`)),
            packetLossPercent: distribution(sampleValues(samples, `${browserTransportPath}.webrtcPacketLossPercent`)),
            packetsLost: sumDeltaOrFinal(samples, `${browserTransportPath}.webrtcPacketsLostDelta`, `${browserTransportPath}.webrtcPacketsLost`),
            framesDropped: sumDeltaOrFinal(samples, `${browserTransportPath}.webrtcFramesDroppedDelta`, `${browserTransportPath}.webrtcFramesDropped`),
            freezes: sumDeltaOrFinal(samples, `${browserTransportPath}.webrtcFreezeCountDelta`, `${browserTransportPath}.webrtcFreezeCount`),
            nackCount: sumDeltaOrFinal(samples, `${browserTransportPath}.webrtcNackCountDelta`, `${browserTransportPath}.webrtcNackCount`),
            pliCount: sumDeltaOrFinal(samples, `${browserTransportPath}.webrtcPliCountDelta`, `${browserTransportPath}.webrtcPliCount`),
        },
        path: {
            iceRoutes: uniquePresent(sampleValues(samples, `${browserTransportPath}.webrtcIceRoute`)),
            candidatePairIds: uniquePresent(sampleValues(samples, `${browserTransportPath}.webrtcSelectedCandidatePairId`)),
            localCandidateTypes: uniquePresent(sampleValues(samples, `${browserTransportPath}.webrtcLocalCandidateType`)),
            localProtocols: uniquePresent(sampleValues(samples, `${browserTransportPath}.webrtcLocalCandidateProtocol`)),
            remoteCandidateTypes: uniquePresent(sampleValues(samples, `${browserTransportPath}.webrtcRemoteCandidateType`)),
            remoteProtocols: uniquePresent(sampleValues(samples, `${browserTransportPath}.webrtcRemoteCandidateProtocol`)),
        },
        freshnessMs: {
            transport: distribution(sampleValues(samples, "freshnessMs.transport")),
            streamerVideo: distribution(sampleValues(samples, "freshnessMs.streamerVideo")),
            streamerVideoTransport: distribution(sampleValues(samples, "freshnessMs.streamerVideoTransport")),
        },
    };
}

async function readJson(filePath, label) {
    try {
        return JSON.parse(await readFile(filePath, "utf8"));
    } catch (error) {
        throw new Error(`Failed to read ${label} at ${filePath}: ${error.message}`, { cause: error });
    }
}

export async function analyzeRun(runDirectory, options = {}) {
    const root = path.resolve(runDirectory);
    const manifestPath = path.join(root, "manifest.json");
    const profilePath = options.profilePath
        ? path.resolve(options.profilePath)
        : path.join(root, "profile.json");

    const manifest = await readJson(manifestPath, "run manifest");
    const profile = await readJson(profilePath, "benchmark profile");
    const errors = [];
    const warnings = [];

    if (manifest.state !== "completed") {
        errors.push(issue("run-not-completed", "Run manifest state is not completed.", { state: manifest.state ?? null }));
    }
    if (manifest.repository?.dirty) {
        const dirtyIssue = issue("repository-dirty", "Run was produced from a dirty repository.", {
            status: manifest.repository.status ?? [],
        });
        if (profile.validityGates?.requireCleanRepository) {
            errors.push(dirtyIssue);
        } else {
            warnings.push(dirtyIssue);
        }
    }

    const browserArtifact = options.browserPath
        ? path.resolve(options.browserPath)
        : manifest.browserExport?.artifact
            ? path.join(root, manifest.browserExport.artifact)
            : path.join(root, "browser-benchmark.json");

    let browser = null;
    let samples = [];
    if (!existsSync(browserArtifact)) {
        errors.push(issue("browser-export-missing", "Browser benchmark export is missing.", { expectedPath: browserArtifact }));
    } else {
        browser = await readJson(browserArtifact, "browser benchmark export");
        if (browser.schemaVersion !== 2) {
            errors.push(issue("browser-schema-unsupported", "Browser benchmark schema version must be 2.", {
                observed: browser.schemaVersion ?? null,
            }));
        }
        samples = Array.isArray(browser.samples) ? browser.samples : [];
        validateSequence(samples, errors);
        validateRunEnvelope(manifest, profile, browser, samples, errors, warnings, root);
        validateProfileAndBrowser(profile, browser, samples, errors, warnings);

        if (browser.sampleSequence) {
            const declared = browser.sampleSequence;
            if (declared.count !== samples.length
                || declared.first !== samples[0]?.sequence
                || declared.last !== samples.at(-1)?.sequence) {
                errors.push(issue("sample-sequence-metadata-mismatch", "sampleSequence metadata does not match the exported samples.", {
                    declared,
                    actual: {
                        count: samples.length,
                        first: samples[0]?.sequence ?? null,
                        last: samples.at(-1)?.sequence ?? null,
                    },
                }));
            }
        }
    }

    const report = {
        schemaVersion: 1,
        analyzedAt: new Date().toISOString(),
        runDirectory: root,
        runId: manifest.runId ?? path.basename(root),
        candidate: manifest.candidate ?? null,
        profile: profile.name ?? null,
        verdict: errors.length === 0 ? "accepted" : "rejected",
        accepted: errors.length === 0,
        errors,
        warnings,
        identity: {
            manifestState: manifest.state ?? null,
            repositoryHead: manifest.repository?.head ?? null,
            repositoryDirty: manifest.repository?.dirty ?? null,
            browserSchemaVersion: browser?.schemaVersion ?? null,
            requestedCodec: profile.stream?.codec ?? null,
            selectedCodec: browser?.selected?.videoCodec ?? null,
        },
        metrics: buildMetrics(samples),
    };

    return report;
}

export function formatReport(report) {
    const lines = [
        `${report.accepted ? "ACCEPT" : "REJECT"} ${report.runId} (${report.candidate ?? "unknown candidate"})`,
        `Profile: ${report.profile ?? "unknown"}`,
        `Samples: ${report.metrics.sampleCount}`,
    ];

    for (const entry of report.errors) {
        lines.push(`ERROR ${entry.code}: ${entry.message}`);
    }
    for (const entry of report.warnings) {
        lines.push(`WARN  ${entry.code}: ${entry.message}`);
    }

    const queueP95 = report.metrics.sender.queueWaitAvgMs.p95;
    const writeP95 = report.metrics.sender.rtpWriteLatencyAvgMs.p95;
    const decodeP95 = report.metrics.browser.decodeTimeMs.p95;
    lines.push(`Sender queue-wait p95: ${queueP95 ?? "n/a"} ms`);
    lines.push(`RTP write-await p95: ${writeP95 ?? "n/a"} ms`);
    lines.push(`Browser decode p95: ${decodeP95 ?? "n/a"} ms`);
    lines.push(`ICE route/protocol: ${report.metrics.path.iceRoutes.join(",") || "n/a"} / ${uniquePresent([
        ...report.metrics.path.localProtocols,
        ...report.metrics.path.remoteProtocols,
    ]).join(",") || "n/a"}`);

    return lines.join("\n");
}

function parseArguments(argv) {
    const options = { json: false, output: null, profilePath: null, browserPath: null, runDirectory: null };
    for (let index = 0; index < argv.length; index += 1) {
        const argument = argv[index];
        if (argument === "--run") options.runDirectory = argv[++index];
        else if (argument === "--profile") options.profilePath = argv[++index];
        else if (argument === "--browser") options.browserPath = argv[++index];
        else if (argument === "--output") options.output = argv[++index];
        else if (argument === "--json") options.json = true;
        else if (argument === "--help" || argument === "-h") options.help = true;
        else throw new Error(`Unknown argument: ${argument}`);
    }
    return options;
}

function usage() {
    return `Usage: node tools/benchmark/analyze-benchmark.mjs --run <run-directory> [options]\n\nOptions:\n  --profile <file>   Override profile.json\n  --browser <file>   Override browser-benchmark.json\n  --output <file>    Write the complete JSON analysis report\n  --json             Print JSON instead of the human report\n  -h, --help         Show this help\n\nExit codes: 0 accepted, 2 rejected, 1 execution/configuration error.`;
}

async function main() {
    try {
        const options = parseArguments(process.argv.slice(2));
        if (options.help) {
            console.log(usage());
            return;
        }
        if (!options.runDirectory) {
            throw new Error("--run is required.\n" + usage());
        }

        const report = await analyzeRun(options.runDirectory, options);
        if (options.output) {
            const outputPath = path.resolve(options.output);
            await writeFile(outputPath, JSON.stringify(report, null, 2) + "\n", "utf8");
        }
        console.log(options.json ? JSON.stringify(report, null, 2) : formatReport(report));
        process.exitCode = report.accepted ? 0 : 2;
    } catch (error) {
        console.error(`Benchmark analysis failed: ${error.message}`);
        process.exitCode = 1;
    }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
    await main();
}
