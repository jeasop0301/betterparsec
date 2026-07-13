import assert from "node:assert/strict"
import test from "node:test"

import {
    CumulativeAverageSampler,
    CumulativeCounterSampler,
    extractSelectedIceCandidatePairStats,
} from "../dist/stream/transport/stats_sampler.js"

test("counter sampler establishes a baseline and reports interval deltas", () => {
    const sampler = new CumulativeCounterSampler()

    assert.equal(sampler.sample("bytes", 1_000, 10_000), null)
    assert.deepEqual(sampler.sample("bytes", 1_500, 11_000), {
        delta: 500,
        elapsedMs: 1_000,
    })
})

test("counter sampler rebases after counter or clock reset", () => {
    const sampler = new CumulativeCounterSampler()

    assert.equal(sampler.sample("packets", 100, 10_000), null)
    assert.equal(sampler.sample("packets", 90, 11_000), null)
    assert.deepEqual(sampler.sample("packets", 95, 12_000), {
        delta: 5,
        elapsedMs: 1_000,
    })
    assert.equal(sampler.sample("packets", 96, 12_000), null)
})

test("average sampler converts cumulative seconds into per-item milliseconds", () => {
    const sampler = new CumulativeAverageSampler()

    assert.equal(sampler.sampleMilliseconds("decode", 1, 100, 10_000), null)
    const averageMs = sampler.sampleMilliseconds("decode", 1.06, 103, 11_000)
    assert.ok(averageMs != null && Math.abs(averageMs - 20) < 1e-9)
    assert.equal(sampler.sampleMilliseconds("decode", 1.06, 103, 12_000), null)
})

test("samplers reject non-finite input", () => {
    const counters = new CumulativeCounterSampler()
    const averages = new CumulativeAverageSampler()

    assert.equal(counters.sample("bytes", Number.NaN, 1), null)
    assert.equal(averages.sampleMilliseconds("decode", 1, Infinity, 1), null)
})

test("ICE stats follow the video transport selected candidate pair", () => {
    const stats = new Map([
        ["inbound-video", {
            id: "inbound-video",
            type: "inbound-rtp",
            kind: "video",
            transportId: "transport-video",
        }],
        ["transport-video", {
            id: "transport-video",
            type: "transport",
            selectedCandidatePairId: "pair-relay",
        }],
        ["pair-direct", {
            id: "pair-direct",
            type: "candidate-pair",
            selected: true,
            state: "succeeded",
            localCandidateId: "local-host",
            remoteCandidateId: "remote-host",
        }],
        ["pair-relay", {
            id: "pair-relay",
            type: "candidate-pair",
            nominated: true,
            state: "succeeded",
            localCandidateId: "local-relay",
            remoteCandidateId: "remote-srflx",
            currentRoundTripTime: 0.0125,
            availableOutgoingBitrate: 8_500_000,
            availableIncomingBitrate: 12_250_000,
        }],
        ["local-host", {
            id: "local-host",
            type: "local-candidate",
            candidateType: "host",
            protocol: "udp",
        }],
        ["remote-host", {
            id: "remote-host",
            type: "remote-candidate",
            candidateType: "host",
            protocol: "udp",
        }],
        ["local-relay", {
            id: "local-relay",
            type: "local-candidate",
            candidateType: "relay",
            protocol: "udp",
            relayProtocol: "tls",
        }],
        ["remote-srflx", {
            id: "remote-srflx",
            type: "remote-candidate",
            candidateType: "srflx",
            protocol: "udp",
        }],
    ])

    assert.deepEqual(extractSelectedIceCandidatePairStats(stats.entries()), {
        webrtcSelectedCandidatePairId: "pair-relay",
        webrtcSelectedCandidatePairSource: "transport",
        webrtcCandidatePairState: "succeeded",
        webrtcLocalCandidateType: "relay",
        webrtcLocalCandidateProtocol: "udp",
        webrtcLocalCandidateRelayProtocol: "tls",
        webrtcRemoteCandidateType: "srflx",
        webrtcRemoteCandidateProtocol: "udp",
        webrtcIceRoute: "relay",
        webrtcCandidatePairCurrentRttMs: 12.5,
        webrtcAvailableOutgoingBitrateKbps: 8_500,
        webrtcAvailableIncomingBitrateKbps: 12_250,
    })
})

test("ICE stats use an unambiguous selected compatibility fallback", () => {
    const stats = new Map([
        ["pair", {
            id: "pair",
            type: "candidate-pair",
            selected: true,
            state: "succeeded",
            localCandidateId: "local",
            remoteCandidateId: "remote",
        }],
        ["local", {
            id: "local",
            type: "local-candidate",
            candidateType: "host",
            protocol: "tcp",
        }],
        ["remote", {
            id: "remote",
            type: "remote-candidate",
            candidateType: "prflx",
            protocol: "tcp",
        }],
    ])

    assert.deepEqual(extractSelectedIceCandidatePairStats(stats.entries()), {
        webrtcSelectedCandidatePairId: "pair",
        webrtcSelectedCandidatePairSource: "selected-flag",
        webrtcCandidatePairState: "succeeded",
        webrtcLocalCandidateType: "host",
        webrtcLocalCandidateProtocol: "tcp",
        webrtcRemoteCandidateType: "prflx",
        webrtcRemoteCandidateProtocol: "tcp",
        webrtcIceRoute: "direct",
    })
})

test("ICE stats refuse to guess between multiple nominated pairs", () => {
    const stats = new Map([
        ["pair-a", {
            id: "pair-a",
            type: "candidate-pair",
            nominated: true,
            state: "succeeded",
        }],
        ["pair-b", {
            id: "pair-b",
            type: "candidate-pair",
            nominated: true,
            state: "succeeded",
        }],
    ])

    assert.deepEqual(extractSelectedIceCandidatePairStats(stats.entries()), {})
})

test("ICE stats omit invalid optional numeric estimates", () => {
    const stats = new Map([
        ["pair", {
            id: "pair",
            type: "candidate-pair",
            selected: true,
            currentRoundTripTime: Number.NaN,
            availableOutgoingBitrate: -1,
            availableIncomingBitrate: Infinity,
        }],
    ])

    assert.deepEqual(extractSelectedIceCandidatePairStats(stats.entries()), {
        webrtcSelectedCandidatePairId: "pair",
        webrtcSelectedCandidatePairSource: "selected-flag",
    })
})
