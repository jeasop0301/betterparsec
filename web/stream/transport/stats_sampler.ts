export type CounterInterval = {
    delta: number
    elapsedMs: number
}

type CounterSample = {
    value: number
    timestampMs: number
}

/**
 * Converts a cumulative RTCStats counter into an interval delta.
 *
 * The first sample and samples following a counter/timestamp reset establish a
 * baseline and intentionally return null. Keeping this state outside the
 * WebRTC transport makes the reset semantics deterministic and independently
 * testable.
 */
export class CumulativeCounterSampler {
    private samples = new Map<string, CounterSample>()

    sample(key: string, value: number, timestampMs: number): CounterInterval | null {
        if (!Number.isFinite(value) || !Number.isFinite(timestampMs)) {
            return null
        }

        const previous = this.samples.get(key)
        this.samples.set(key, { value, timestampMs })

        if (!previous || value < previous.value || timestampMs <= previous.timestampMs) {
            return null
        }

        return {
            delta: value - previous.value,
            elapsedMs: timestampMs - previous.timestampMs,
        }
    }

    clear(): void {
        this.samples.clear()
    }
}

type AverageSample = {
    totalSeconds: number
    count: number
    timestampMs: number
}

/** Produces a per-item interval average from cumulative seconds and count. */
export class CumulativeAverageSampler {
    private samples = new Map<string, AverageSample>()

    sampleMilliseconds(
        key: string,
        totalSeconds: number,
        count: number,
        timestampMs: number,
    ): number | null {
        if (!Number.isFinite(totalSeconds)
            || !Number.isFinite(count)
            || !Number.isFinite(timestampMs)) {
            return null
        }

        const previous = this.samples.get(key)
        this.samples.set(key, { totalSeconds, count, timestampMs })

        if (!previous
            || totalSeconds < previous.totalSeconds
            || count < previous.count
            || timestampMs <= previous.timestampMs) {
            return null
        }

        const countDelta = count - previous.count
        if (countDelta <= 0) {
            return null
        }

        return (totalSeconds - previous.totalSeconds) * 1000 / countDelta
    }

    clear(): void {
        this.samples.clear()
    }
}

type RtcStatsRecord = Record<string, unknown>
type ExportedStatValue = string | number

function finiteNumber(value: unknown): number | null {
    return typeof value == "number" && Number.isFinite(value) ? value : null
}

function stringValue(value: unknown): string | null {
    return typeof value == "string" && value.length > 0 ? value : null
}

/**
 * Extracts the selected ICE path from an RTCStats report.
 *
 * The transport report's selectedCandidatePairId is the standards-defined
 * source of truth. Some browsers omit that link, so a single unambiguous
 * selected or nominated+succeeded pair is accepted as a compatibility
 * fallback. Multiple fallback candidates deliberately produce no result.
 */
export function extractSelectedIceCandidatePairStats(
    entries: Iterable<[string, RtcStatsRecord]>,
): Record<string, ExportedStatValue> {
    const reports = new Map<string, RtcStatsRecord>()
    const uniqueReports: RtcStatsRecord[] = []

    for (const [key, report] of entries) {
        reports.set(key, report)
        const reportId = stringValue(report.id)
        if (reportId) {
            reports.set(reportId, report)
        }
        uniqueReports.push(report)
    }

    const videoTransportIds = uniqueReports
        .filter(report => report.type == "inbound-rtp"
            && (report.kind ?? report.mediaType) == "video")
        .map(report => stringValue(report.transportId))
        .filter((id): id is string => id != null)

    const allTransportIds = uniqueReports
        .filter(report => report.type == "transport")
        .map(report => stringValue(report.id))
        .filter((id): id is string => id != null)

    let pair: RtcStatsRecord | null = null
    let pairId: string | null = null
    let selectionSource: string | null = null

    for (const transportId of [...videoTransportIds, ...allTransportIds]) {
        const transport = reports.get(transportId)
        const selectedPairId = stringValue(transport?.selectedCandidatePairId)
        const selectedPair = selectedPairId ? reports.get(selectedPairId) : null
        if (selectedPair?.type == "candidate-pair") {
            pair = selectedPair
            pairId = selectedPairId
            selectionSource = "transport"
            break
        }
    }

    const candidatePairs = uniqueReports.filter(report => report.type == "candidate-pair")
    if (!pair) {
        const explicitlySelected = candidatePairs.filter(report => report.selected === true)
        if (explicitlySelected.length == 1) {
            pair = explicitlySelected[0]
            pairId = stringValue(pair.id)
            selectionSource = "selected-flag"
        }
    }
    if (!pair) {
        const nominatedAndSucceeded = candidatePairs.filter(report => report.nominated === true
            && report.state == "succeeded")
        if (nominatedAndSucceeded.length == 1) {
            pair = nominatedAndSucceeded[0]
            pairId = stringValue(pair.id)
            selectionSource = "nominated"
        }
    }

    if (!pair) {
        return {}
    }

    const result: Record<string, ExportedStatValue> = {}
    if (pairId) {
        result.webrtcSelectedCandidatePairId = pairId
    }
    if (selectionSource) {
        result.webrtcSelectedCandidatePairSource = selectionSource
    }

    const pairState = stringValue(pair.state)
    if (pairState) {
        result.webrtcCandidatePairState = pairState
    }

    const localCandidateId = stringValue(pair.localCandidateId)
    const remoteCandidateId = stringValue(pair.remoteCandidateId)
    const localCandidate = localCandidateId ? reports.get(localCandidateId) : null
    const remoteCandidate = remoteCandidateId ? reports.get(remoteCandidateId) : null

    const candidateFields = [
        [localCandidate, "Local"],
        [remoteCandidate, "Remote"],
    ] as const

    for (const [candidate, prefix] of candidateFields) {
        if (!candidate) {
            continue
        }

        const candidateType = stringValue(candidate.candidateType)
        const protocol = stringValue(candidate.protocol)
        const relayProtocol = stringValue(candidate.relayProtocol)
        if (candidateType) {
            result[`webrtc${prefix}CandidateType`] = candidateType
        }
        if (protocol) {
            result[`webrtc${prefix}CandidateProtocol`] = protocol
        }
        if (relayProtocol) {
            result[`webrtc${prefix}CandidateRelayProtocol`] = relayProtocol
        }
    }

    const localType = stringValue(localCandidate?.candidateType)
    const remoteType = stringValue(remoteCandidate?.candidateType)
    if (localType && remoteType) {
        result.webrtcIceRoute = localType == "relay" || remoteType == "relay"
            ? "relay"
            : "direct"
    }

    const currentRoundTripTime = finiteNumber(pair.currentRoundTripTime)
    if (currentRoundTripTime != null && currentRoundTripTime >= 0) {
        result.webrtcCandidatePairCurrentRttMs = currentRoundTripTime * 1000
    }

    const availableOutgoingBitrate = finiteNumber(pair.availableOutgoingBitrate)
    if (availableOutgoingBitrate != null && availableOutgoingBitrate >= 0) {
        result.webrtcAvailableOutgoingBitrateKbps = availableOutgoingBitrate / 1000
    }

    const availableIncomingBitrate = finiteNumber(pair.availableIncomingBitrate)
    if (availableIncomingBitrate != null && availableIncomingBitrate >= 0) {
        result.webrtcAvailableIncomingBitrateKbps = availableIncomingBitrate / 1000
    }

    return result
}
